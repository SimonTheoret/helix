//! PTY transport and terminal protocol state. All terminal implementation lives in Rust.
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{color, Config, Osc52, RenderableCursor, TermMode};
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use alacritty_terminal::Term;
use anyhow::{bail, Context, Result};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

const QUEUE_SIZE: usize = 128;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const SCROLLBACK_LINES: usize = 10_000;

/// Forward protocol replies to the PTY without borrowing the terminal from its
/// own event listener. Clipboard and window-management events stay disabled.
struct TerminalEvents(mpsc::Sender<Event>);

impl EventListener for TerminalEvents {
    fn send_event(&self, event: Event) {
        if matches!(event, Event::PtyWrite(_) | Event::ColorRequest(..) | Event::TextAreaSizeRequest(_)) {
            let _ = self.0.send(event);
        }
    }
}

struct Size {
    rows: u16,
    cols: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Alacritty's parser, grid, modes, scrollback, and protocol responses.
pub(crate) struct Screen {
    term: Term<TerminalEvents>,
    processor: Processor,
    events: mpsc::Receiver<Event>,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::with_history(rows, cols, SCROLLBACK_LINES)
    }

    fn with_history(rows: u16, cols: u16, history: usize) -> Self {
        let (tx, events) = mpsc::channel();
        let config = Config {
            scrolling_history: history,
            osc52: Osc52::Disabled,
            // Input is encoded using the legacy xterm protocol below.
            kitty_keyboard: false,
            ..Config::default()
        };
        let size = Size { rows: rows.max(1), cols: cols.max(1) };
        Self {
            term: Term::new(config, &size, TerminalEvents(tx)),
            processor: Processor::new(),
            events,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut replies = self.flush_sync_timeout(Instant::now());
        for byte in bytes {
            self.processor.advance(&mut self.term, *byte);
            // Handle requests in stream order, before a later sequence can
            // change a requested color. This also bounds the event queue.
            self.drain_events(&mut replies);
        }
        replies
    }

    fn drain_events(&mut self, replies: &mut Vec<u8>) {
        while let Ok(event) = self.events.try_recv() {
            let text = match event {
                Event::PtyWrite(text) => text,
                Event::ColorRequest(index, format) => format(self.color_rgb(index)),
                Event::TextAreaSizeRequest(format) => {
                    let (rows, cols) = self.size();
                    format(WindowSize {
                        num_lines: rows,
                        num_cols: cols,
                        // The outer terminal does not supply cell pixel sizes.
                        cell_width: 0,
                        cell_height: 0,
                    })
                }
                _ => continue,
            };
            replies.extend_from_slice(text.as_bytes());
        }
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.processor.sync_timeout().sync_timeout()
    }

    fn flush_sync_timeout(&mut self, now: Instant) -> Vec<u8> {
        if self.sync_deadline().is_some_and(|deadline| deadline <= now) {
            self.processor.stop_sync(&mut self.term);
        }
        let mut replies = Vec::new();
        self.drain_events(&mut replies);
        replies
    }

    pub fn size(&self) -> (u16, u16) {
        (self.term.screen_lines() as u16, self.term.columns() as u16)
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.term.resize(Size { rows: rows.max(1), cols: cols.max(1) });
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn scroll(&mut self, scroll: Scroll) {
        if !self.mode().contains(TermMode::ALT_SCREEN) {
            self.term.scroll_display(scroll);
        }
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        let (rows, cols) = self.size();
        if row >= rows || col >= cols {
            return None;
        }
        let line = Line(row as i32 - self.scroll_offset() as i32);
        Some(&self.term.grid()[Point::new(line, Column(col as usize))])
    }

    pub fn cursor(&self) -> RenderableCursor {
        self.term.renderable_content().cursor
    }

    pub fn colors(&self) -> &color::Colors {
        self.term.colors()
    }

    /// Resolve OSC palette overrides, with xterm defaults for color queries.
    pub fn color_rgb(&self, index: usize) -> Rgb {
        if index < color::COUNT {
            if let Some(rgb) = self.colors()[index] {
                return rgb;
            }
        }
        const ANSI: [[u8; 3]; 16] = [
            [0, 0, 0], [205, 0, 0], [0, 205, 0], [205, 205, 0],
            [0, 0, 238], [205, 0, 205], [0, 205, 205], [229, 229, 229],
            [127, 127, 127], [255, 0, 0], [0, 255, 0], [255, 255, 0],
            [92, 92, 255], [255, 0, 255], [0, 255, 255], [255, 255, 255],
        ];
        let [r, g, b] = match index {
            0..=15 => ANSI[index],
            16..=231 => {
                let value = index - 16;
                let component = |n: usize| if n == 0 { 0 } else { (55 + n * 40) as u8 };
                [component(value / 36), component(value / 6 % 6), component(value % 6)]
            }
            232..=255 => [8 + (index as u8 - 232) * 10; 3],
            257 => [0, 0, 0],
            259..=266 => ANSI[index - 259].map(|component| component / 2),
            267 => [255, 255, 255],
            268 => [128, 128, 128],
            _ => [208, 208, 208],
        };
        Rgb { r, g, b }
    }

    #[cfg(test)]
    fn contents(&self) -> String {
        let (rows, cols) = self.size();
        let mut text = String::new();
        for row in 0..rows {
            for col in 0..cols {
                let cell = self.cell(row, col).unwrap();
                if !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    text.push_str(&cell_text(cell));
                }
            }
            text.push('\n');
        }
        text
    }
}

pub(crate) fn cell_text(cell: &Cell) -> String {
    if cell.flags.contains(Flags::HIDDEN) || cell.c == '\t' {
        return " ".to_owned();
    }
    let mut text = cell.c.to_string();
    if let Some(zerowidth) = cell.zerowidth() {
        text.extend(zerowidth.iter().copied());
    }
    text
}

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
    pub screen: Screen,
    pub exit: Option<String>,
    pub error: Option<String>,
    wake: Arc<dyn Fn() + Send + Sync>,
    sync_timer: Option<(Instant, tokio::task::JoinHandle<()>)>,
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

        let reaper_wake = wake.clone();
        thread::spawn(move || {
            let event = match child.wait() {
                Ok(status) => Output::Exit(status.to_string()),
                Err(err) => Output::Error(format!("Terminal wait: {err}")),
            };
            let _ = output_tx.send(event);
            reaper_wake();
        });

        Ok(Self {
            master: pair.master,
            killer,
            input: input_tx,
            output: output_rx,
            screen: Screen::new(rows, cols),
            exit: None,
            error: None,
            wake,
            sync_timer: None,
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
                Ok(Output::Data(bytes)) => {
                    let replies = self.screen.process(&bytes);
                    if let Err(err) = self.send(replies) {
                        self.error = Some(err.to_string());
                    }
                }
                Ok(Output::Exit(status)) => self.exit = Some(status),
                Ok(Output::Error(err)) => self.error = Some(err),
                Err(_) => break,
            }
        }
        let replies = self.screen.flush_sync_timeout(Instant::now());
        if let Err(err) = self.send(replies) {
            self.error = Some(err.to_string());
        }
        // A client may begin a synchronized update without ending it or sending
        // more output. Wake the editor when Alacritty's timeout expires.
        let deadline = self.screen.sync_deadline();
        if self.sync_timer.as_ref().map(|timer| timer.0) != deadline {
            if let Some((_, task)) = self.sync_timer.take() {
                task.abort();
            }
            if let (Some(deadline), Ok(runtime)) = (deadline, tokio::runtime::Handle::try_current()) {
                let wake = self.wake.clone();
                let task = runtime.spawn(async move {
                    tokio::time::sleep_until(deadline.into()).await;
                    wake();
                });
                self.sync_timer = Some((deadline, task));
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
        if self.screen.size() != (rows, cols) {
            self.master.resize(pty_size(rows, cols))?;
            self.screen.resize(rows, cols);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some((_, task)) = self.sync_timer.take() {
            task.abort();
        }
        if self.exit.is_none() {
            let _ = self.killer.kill();
        }
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
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
        use alacritty_terminal::vte::ansi::{Color, NamedColor};
        let mut screen = Screen::new(3, 20);
        for byte in "\x1b[31m界e\u{301}".as_bytes() {
            screen.process(&[*byte]);
        }
        assert_eq!(screen.cell(0, 0).unwrap().c, '界');
        assert!(screen.cell(0, 1).unwrap().flags.contains(Flags::WIDE_CHAR_SPACER));
        assert_eq!(cell_text(screen.cell(0, 2).unwrap()), "e\u{301}");
        assert_eq!(screen.cell(0, 0).unwrap().fg, Color::Named(NamedColor::Red));
        assert_eq!(screen.process(b"\x1b[6n\x1b[5n\x1b[c"), b"\x1b[1;4R\x1b[0n\x1b[?6c");
        screen.process(b"\x1b[?1049hOTHER\x1b[?1049l");
        assert!(screen.contents().contains("界e\u{301}"));
    }

    #[test]
    fn scrollback_is_bounded_and_resize_preserves_state() {
        let mut screen = Screen::with_history(2, 12, 3);
        for line in 0..20 {
            screen.process(format!("line{line}\r\n").as_bytes());
        }
        screen.scroll(Scroll::Top);
        assert_eq!(screen.scroll_offset(), 3);
        screen.scroll(Scroll::Bottom);
        screen.resize(4, 20);
        assert_eq!(screen.size(), (4, 20));
        assert!(screen.contents().contains("line19"));
    }

    #[test]
    fn palette_queries_modes_and_cursor_styles() {
        use alacritty_terminal::vte::ansi::CursorShape;
        let mut screen = Screen::new(3, 20);
        assert_eq!(
            screen.process(b"\x1b]4;1;rgb:12/34/56\x07\x1b]4;1;?\x07"),
            b"\x1b]4;1;rgb:1212/3434/5656\x07"
        );
        screen.process(b"\x1b[?1h\x1b[?2004h\x1b[6 q");
        assert!(screen.mode().contains(TermMode::APP_CURSOR | TermMode::BRACKETED_PASTE));
        assert_eq!(screen.cursor().shape, CursorShape::Beam);
        screen.process(b"\x1b[?25l");
        assert_eq!(screen.cursor().shape, CursorShape::Hidden);
        // Clipboard queries must not read or change the editor's clipboard.
        assert!(screen.process(b"\x1b]52;c;?\x07").is_empty());
        assert_eq!(screen.process(b"\x1b[18t"), b"\x1b[8;3;20t");
    }

    #[test]
    fn synchronized_output_is_flushed_on_timeout() {
        let mut screen = Screen::new(2, 20);
        screen.process(b"\x1b[?2026hhello");
        assert_eq!(screen.cell(0, 0).unwrap().c, ' ');
        let deadline = screen.sync_deadline().expect("synchronized output timeout");
        screen.flush_sync_timeout(deadline);
        assert!(screen.contents().contains("hello"));
        assert!(screen.sync_deadline().is_none());
    }

    #[test]
    fn widening_reflows_wrapped_text() {
        let mut screen = Screen::new(3, 6);
        screen.process(b"abcdefghij");
        screen.resize(3, 12);
        assert!(screen.contents().contains("abcdefghij"));
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_input_resize_and_exit() {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "stty -echo; printf READY; read value; stty size; printf '<%s>' \"$value\""]);
        let mut session = Session::spawn(cmd, 4, 40, Arc::new(|| {})).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !session.screen.contents().contains("READY") {
            session.poll();
            assert!(std::time::Instant::now() < deadline, "shell did not start");
            thread::sleep(std::time::Duration::from_millis(10));
        }
        session.resize(6, 50).unwrap();
        session.send(b"hello\n".to_vec()).unwrap();
        loop {
            session.poll();
            let text = session.screen.contents();
            if text.contains("<hello>") && session.exit.is_some() {
                assert!(text.contains("6 50"));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "missing PTY output: {text}");
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
