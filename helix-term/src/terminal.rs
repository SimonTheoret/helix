//! PTY transport and terminal protocol state. All terminal implementation lives in Rust.
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::thread;

use anyhow::{bail, Context, Result};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

const QUEUE_SIZE: usize = 128;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const SCROLLBACK_LINES: usize = 10_000;

enum Output {
    Data(Vec<u8>),
    Exit(String),
    Error(String),
}

/// Owns a shell, its PTY, and bounded input/output queues. Blocking IO never runs
/// on the editor thread. Dropping a session kills its child; the waiter reaps it.
pub(crate) struct Session {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    input: mpsc::SyncSender<Vec<u8>>,
    output: mpsc::Receiver<Output>,
    pub parser: vt100::Parser<Replies>,
    pub exit: Option<String>,
    pub error: Option<String>,
}

impl Session {
    pub fn spawn(
        command: CommandBuilder,
        rows: u16,
        cols: u16,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let pair = native_pty_system()
            .openpty(pty_size(rows, cols))
            .context("open terminal PTY")?;
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let mut child = pair.slave.spawn_command(command).context("start terminal shell")?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(16);
        let (output_tx, output_rx) = mpsc::sync_channel(QUEUE_SIZE);

        let tx = output_tx.clone();
        let notify = wake.clone();
        thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(len) => {
                        if tx.send(Output::Data(buf[..len].to_vec())).is_err() {
                            break;
                        }
                        notify();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Linux reports EIO when the last slave closes.
                    #[cfg(unix)]
                    Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
                    Err(err) => {
                        let _ = tx.send(Output::Error(format!("Terminal read: {err}")));
                        notify();
                        break;
                    }
                }
            }
        });

        let tx = output_tx.clone();
        let notify = wake.clone();
        thread::spawn(move || {
            while let Ok(bytes) = input_rx.recv() {
                if let Err(err) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    let _ = tx.send(Output::Error(format!("Terminal write: {err}")));
                    notify();
                    break;
                }
            }
        });

        thread::spawn(move || {
            let event = match child.wait() {
                Ok(status) => Output::Exit(status.to_string()),
                Err(err) => Output::Error(format!("Terminal wait: {err}")),
            };
            let _ = output_tx.send(event);
            wake();
        });

        Ok(Self {
            master: pair.master,
            killer,
            input: input_tx,
            output: output_rx,
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, Replies::default()),
            exit: None,
            error: None,
        })
    }

    pub fn shell(cwd: &Path, rows: u16, cols: u16, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        let shell_var = if cfg!(windows) { "COMSPEC" } else { "SHELL" };
        let mut command = match std::env::var_os(shell_var).filter(|s| !s.is_empty()) {
            Some(shell) => CommandBuilder::new(shell),
            None => CommandBuilder::new_default_prog(),
        };
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // These describe the parent terminal, not the emulated one.
        for name in ["TERM_PROGRAM", "TERM_PROGRAM_VERSION", "COLUMNS", "LINES"] {
            command.env_remove(name);
        }
        Self::spawn(command, rows, cols, wake)
    }

    /// Bound the amount of parsing per frame as well as queued output memory.
    pub fn poll(&mut self) {
        for _ in 0..QUEUE_SIZE {
            match self.output.try_recv() {
                Ok(Output::Data(bytes)) => self.parser.process(&bytes),
                Ok(Output::Exit(status)) => self.exit = Some(status),
                Ok(Output::Error(err)) => self.error = Some(err),
                Err(_) => break,
            }
        }
        let replies = std::mem::take(&mut self.parser.callbacks_mut().bytes);
        if !replies.is_empty() {
            if let Err(err) = self.send(replies) {
                self.error = Some(err.to_string());
            }
        }
    }

    pub fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.is_empty() || self.exit.is_some() {
            return Ok(());
        }
        if bytes.len() > MAX_INPUT_BYTES {
            bail!("Terminal input exceeds 1 MiB");
        }
        self.input.try_send(bytes).map_err(|err| match err {
            mpsc::TrySendError::Full(_) => anyhow::anyhow!("Terminal input queue is full; retry input"),
            mpsc::TrySendError::Disconnected(_) => anyhow::anyhow!("Terminal input is closed"),
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.parser.screen().size() != (rows, cols) {
            self.master.resize(pty_size(rows, cols))?;
            self.parser.screen_mut().set_size(rows, cols);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.exit.is_none() {
            let _ = self.killer.kill();
        }
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

/// Reply to queries used by shells and full-screen applications. OSC clipboard
/// requests intentionally use the parser's default no-op implementation.
#[derive(Default)]
pub(crate) struct Replies {
    bytes: Vec<u8>,
}

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if i2.is_some() || self.bytes.len() > 8192 {
            return;
        }
        let param = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let response = match (i1, command, param) {
            (None, 'n', 5) => "\x1b[0n".to_owned(),
            (None | Some(b'?'), 'n', 6) => {
                let (row, col) = screen.cursor_position();
                let private = if i1.is_some() { "?" } else { "" };
                format!("\x1b[{private}{};{}R", row + 1, col + 1)
            }
            (None, 'c', 0) => "\x1b[?1;2c".to_owned(),
            (Some(b'>'), 'c', 0) => "\x1b[>0;0;0c".to_owned(),
            (None, 't', 18) => {
                let (rows, cols) = screen.size();
                format!("\x1b[8;{rows};{cols}t")
            }
            _ => return,
        };
        self.bytes.extend_from_slice(response.as_bytes());
    }
}

pub(crate) fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // A pasted terminator must not break out of the paste envelope.
        let text = text.replace('\x1b', "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Encode legacy xterm input, including application cursor mode and modifiers.
pub(crate) fn encode_key(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    use KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if key.modifiers.contains(KeyModifiers::SUPER) {
        return Vec::new();
    }
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let mut bytes = Vec::new();
    let sequence = match key.code {
        Char(ch) => {
            if alt { bytes.push(0x1b); }
            if ctrl {
                match ch.to_ascii_lowercase() {
                    'a'..='z' => bytes.push(ch.to_ascii_lowercase() as u8 - b'a' + 1),
                    ' ' | '@' | '2' => bytes.push(0),
                    '[' | '3' => bytes.push(0x1b),
                    '\\' | '4' => bytes.push(0x1c),
                    ']' | '5' => bytes.push(0x1d),
                    '^' | '6' => bytes.push(0x1e),
                    '_' | '/' | '7' => bytes.push(0x1f),
                    '?' | '8' => bytes.push(0x7f),
                    _ => return Vec::new(),
                }
            } else {
                let mut buf = [0; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            return bytes;
        }
        Up | Down | Right | Left | Home | End => {
            let end = match key.code {
                Up => 'A', Down => 'B', Right => 'C', Left => 'D', Home => 'H', _ => 'F',
            };
            if modifier != 1 { format!("\x1b[1;{modifier}{end}") }
            else if application_cursor { format!("\x1bO{end}") }
            else { format!("\x1b[{end}") }
        }
        Insert | Delete | PageUp | PageDown | F(5..=12) => {
            let number = match key.code {
                Insert => 2, Delete => 3, PageUp => 5, PageDown => 6,
                F(5) => 15, F(6) => 17, F(7) => 18, F(8) => 19,
                F(9) => 20, F(10) => 21, F(11) => 23, _ => 24,
            };
            if modifier == 1 { format!("\x1b[{number}~") }
            else { format!("\x1b[{number};{modifier}~") }
        }
        F(1..=4) => {
            let F(number) = key.code else { unreachable!() };
            let end = char::from(b'P' + number - 1);
            if modifier == 1 { format!("\x1bO{end}") }
            else { format!("\x1b[1;{modifier}{end}") }
        }
        Enter | Backspace | Tab | Esc | Null => {
            if alt { bytes.push(0x1b); }
            bytes.extend_from_slice(match key.code {
                Enter => b"\r", Backspace if ctrl => b"\x08", Backspace => b"\x7f",
                Tab if shift => b"\x1b[Z", Tab => b"\t", Esc => b"\x1b", _ => b"\0",
            });
            return bytes;
        }
        _ => return Vec::new(),
    };
    sequence.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent { code, modifiers }
    }

    #[test]
    fn keyboard_protocol() {
        use KeyCode::*;
        use KeyModifiers as M;
        assert_eq!(encode_key(key(Char('c'), M::CONTROL), false), b"\x03");
        assert_eq!(encode_key(key(Char('C'), M::CONTROL | M::SHIFT), false), b"\x03");
        assert_eq!(encode_key(key(Char('é'), M::ALT), false), "\x1bé".as_bytes());
        assert_eq!(encode_key(key(Up, M::NONE), false), b"\x1b[A");
        assert_eq!(encode_key(key(Up, M::NONE), true), b"\x1bOA");
        assert_eq!(encode_key(key(Left, M::CONTROL), true), b"\x1b[1;5D");
        assert_eq!(encode_key(key(F(12), M::SHIFT), false), b"\x1b[24;2~");
        assert_eq!(encode_key(key(Tab, M::SHIFT), false), b"\x1b[Z");
        assert_eq!(encode_key(key(Char('\\'), M::CONTROL), false), b"\x1c");
    }

    #[test]
    fn paste_protocol() {
        assert_eq!(paste("a\r\nb\nc", false), b"a\rb\rc");
        assert_eq!(paste("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste("\x1b[201~echo", true), b"\x1b[200~[201~echo\x1b[201~");
    }

    #[test]
    fn fragmented_unicode_colors_queries_and_alternate_screen() {
        let mut parser = vt100::Parser::new_with_callbacks(3, 20, 100, Replies::default());
        for byte in "\x1b[31m界e\u{301}".as_bytes() {
            parser.process(&[*byte]);
        }
        assert_eq!(parser.screen().cell(0, 0).unwrap().contents(), "界");
        assert!(parser.screen().cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(parser.screen().cell(0, 2).unwrap().contents(), "e\u{301}");
        assert_eq!(parser.screen().cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
        parser.process(b"\x1b[6n\x1b[5n\x1b[c");
        assert_eq!(parser.callbacks().bytes, b"\x1b[1;4R\x1b[0n\x1b[?1;2c");
        parser.process(b"\x1b[?1049hOTHER\x1b[?1049l");
        assert!(parser.screen().contents().contains("界e\u{301}"));
    }

    #[test]
    fn scrollback_is_bounded_and_resize_preserves_state() {
        let mut parser = vt100::Parser::new(2, 12, 3);
        for line in 0..20 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(parser.screen().scrollback(), 3);
        parser.screen_mut().set_scrollback(0);
        parser.screen_mut().set_size(4, 20);
        assert_eq!(parser.screen().size(), (4, 20));
        assert!(parser.screen().contents().contains("line19"));
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_input_resize_and_exit() {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "stty -echo; printf READY; read value; stty size; printf '<%s>' \"$value\""]);
        let mut session = Session::spawn(cmd, 4, 40, Arc::new(|| {})).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !session.parser.screen().contents().contains("READY") {
            session.poll();
            assert!(std::time::Instant::now() < deadline, "shell did not start");
            thread::sleep(std::time::Duration::from_millis(10));
        }
        session.resize(6, 50).unwrap();
        session.send(b"hello\n".to_vec()).unwrap();
        loop {
            session.poll();
            let text = session.parser.screen().contents();
            if text.contains("<hello>") && session.exit.is_some() {
                assert!(text.contains("6 50"));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "missing PTY output: {text}");
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
