//! A persistent terminal layer. Hiding it preserves the shell and scrollback.
use std::sync::Arc;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color as TerminalColor, CursorShape, NamedColor};
use helix_core::Position;
use helix_view::graphics::{Color, CursorKind, Modifier, Rect, Style, UnderlineStyle};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use helix_view::Editor;
use tui::buffer::Buffer;

use crate::compositor::{Component, Context, Event, EventResult};
use crate::terminal::{cell_text, encode_key, paste, Screen, Session};

pub(crate) const ID: &str = "terminal";

pub(crate) struct Terminal {
    session: Session,
    pub visible: bool,
    prefix: bool,
    area: Rect,
}

impl Terminal {
    pub fn new(area: Rect) -> anyhow::Result<Self> {
        let body = body_area(area);
        // Runtime-local redraw notifications also work in integration tests.
        let runtime = tokio::runtime::Handle::current();
        let wake = Arc::new(move || {
            let _guard = runtime.enter();
            helix_event::request_redraw();
        });
        let session = Session::shell(
            &std::env::current_dir()?,
            body.height,
            body.width,
            wake,
        )?;
        Ok(Self { session, visible: true, prefix: false, area })
    }

    fn send(&mut self, bytes: Vec<u8>, ctx: &mut Context) {
        self.session.screen.scroll(Scroll::Bottom);
        if let Err(err) = self.session.send(bytes) {
            ctx.editor.set_error(err.to_string());
        }
    }

    fn scroll(&mut self, up: bool, amount: usize) {
        let amount = amount.min(i32::MAX as usize) as i32;
        self.session.screen.scroll(Scroll::Delta(if up { amount } else { -amount }));
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Context) -> EventResult {
        let control = |ch| key.code == KeyCode::Char(ch) && key.modifiers == KeyModifiers::CONTROL;
        if self.prefix {
            self.prefix = false;
            if control('n') {
                self.visible = false;
                return EventResult::Consumed(None);
            }
            if control('q') {
                return EventResult::Consumed(Some(Box::new(|compositor, _| {
                    compositor.remove(ID);
                })));
            }
            self.send(vec![0x1c], ctx);
            if control('\\') {
                return EventResult::Consumed(None);
            }
        } else if control('\\') {
            self.prefix = true;
            return EventResult::Consumed(None);
        }
        if key.modifiers == KeyModifiers::SHIFT {
            match key.code {
                KeyCode::PageUp => {
                    self.scroll(true, body_area(self.area).height as usize);
                    return EventResult::Consumed(None);
                }
                KeyCode::PageDown => {
                    self.scroll(false, body_area(self.area).height as usize);
                    return EventResult::Consumed(None);
                }
                _ => {}
            }
        }
        let application_cursor = self.session.screen.mode().contains(TermMode::APP_CURSOR);
        let mut bytes = encode_key(key, application_cursor);
        if key.code == KeyCode::Enter
            && self.session.screen.mode().contains(TermMode::LINE_FEED_NEW_LINE)
        {
            bytes.push(b'\n');
        }
        self.send(bytes, ctx);
        EventResult::Consumed(None)
    }
}

impl Component for Terminal {
    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }

    fn records_macro_keys(&self) -> bool {
        !self.visible
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        self.session.poll();
        if !self.visible {
            return EventResult::Ignored(None);
        }
        match event {
            Event::Key(key) => return self.key(*key, ctx),
            Event::Paste(text) => {
                let bracketed = self.session.screen.mode().contains(TermMode::BRACKETED_PASTE);
                self.send(paste(text, bracketed), ctx);
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll(true, 3),
                MouseEventKind::ScrollDown => self.scroll(false, 3),
                _ => {}
            },
            Event::Resize(_, _) => return EventResult::Ignored(None),
            Event::FocusLost => self.prefix = false,
            _ => {}
        }
        EventResult::Consumed(None)
    }

    fn render(&mut self, area: Rect, frame: &mut Buffer, ctx: &mut Context) {
        // Drain output even while hidden so a busy shell cannot block forever.
        self.session.poll();
        self.area = area;
        if !self.visible || area.width == 0 || area.height == 0 {
            return;
        }
        let body = body_area(area);
        if let Err(err) = self.session.resize(body.height, body.width) {
            self.session.error = Some(format!("Terminal resize: {err}"));
        }
        let header_y = body.y.saturating_sub(1).max(area.y);
        let header_style = ctx.editor.theme.get("ui.statusline");
        for x in area.x..area.right() {
            let cell = frame.get_mut(x, header_y);
            cell.reset();
            cell.set_style(header_style);
        }
        let state = self.session.error.as_deref().or(self.session.exit.as_deref());
        let title = match state {
            Some(state) => format!(" Terminal [{state}] | C-\\ C-n hide | C-\\ C-q close"),
            None if self.prefix => " Terminal | C-n hide | C-q close | C-\\ send prefix".to_owned(),
            None => " Terminal | C-\\ C-n hide | C-\\ C-q close | Shift-PgUp/PgDn scroll".to_owned(),
        };
        frame.set_stringn(area.x, header_y, title, area.width as usize, header_style);

        let screen = &self.session.screen;
        for row in 0..body.height {
            for col in 0..body.width {
                let target = frame.get_mut(body.x + col, body.y + row);
                target.reset();
                if let Some(source) = screen.cell(row, col) {
                    // The renderer accounts for wide glyphs; do not draw their
                    // continuation cells as independent glyphs.
                    if !source.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                        target.set_symbol(&cell_text(source));
                    }
                    target.set_style(cell_style(source, screen));
                }
            }
        }
    }

    fn cursor(&self, area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        if !self.visible {
            return (None, CursorKind::Hidden);
        }
        let body = body_area(area);
        let screen = &self.session.screen;
        let cursor = screen.cursor();
        let row = cursor.point.line.0.max(0) as u16;
        let col = cursor.point.column.0 as u16;
        let hidden = screen.scroll_offset() != 0 || body.height == 0 || self.session.exit.is_some();
        let position = Position::new(
            (body.y + row.min(body.height.saturating_sub(1)))
                .min(area.bottom().saturating_sub(1)).max(area.y) as usize,
            (body.x + col.min(body.width.saturating_sub(1)))
                .min(area.right().saturating_sub(1)).max(area.x) as usize,
        );
        // Some(position) also prevents the underlying editor cursor showing
        // through when an application hides its own cursor.
        let kind = if hidden {
            CursorKind::Hidden
        } else {
            match cursor.shape {
                CursorShape::Block | CursorShape::HollowBlock => CursorKind::Block,
                CursorShape::Beam => CursorKind::Bar,
                CursorShape::Underline => CursorKind::Underline,
                CursorShape::Hidden => CursorKind::Hidden,
            }
        };
        (Some(position), kind)
    }
}

fn body_area(area: Rect) -> Rect {
    let height = area.height.saturating_sub(1) / 2;
    Rect { x: area.x, y: area.bottom() - height, width: area.width, height }
}

fn color(color: TerminalColor, screen: &Screen) -> Color {
    let index = match color {
        TerminalColor::Spec(rgb) => return Color::Rgb(rgb.r, rgb.g, rgb.b),
        TerminalColor::Indexed(index) => index as usize,
        TerminalColor::Named(named) => named as usize,
    };
    if let Some(rgb) = screen.colors()[index] {
        return Color::Rgb(rgb.r, rgb.g, rgb.b);
    }
    match color {
        TerminalColor::Named(NamedColor::Foreground | NamedColor::Background) => Color::Reset,
        _ if index < 256 => Color::Indexed(index as u8),
        _ => {
            let rgb = screen.color_rgb(index);
            Color::Rgb(rgb.r, rgb.g, rgb.b)
        }
    }
}

fn cell_style(cell: &Cell, screen: &Screen) -> Style {
    let mut modifiers = Modifier::empty();
    for (flag, modifier) in [
        (Flags::BOLD, Modifier::BOLD),
        (Flags::DIM, Modifier::DIM),
        (Flags::ITALIC, Modifier::ITALIC),
        (Flags::INVERSE, Modifier::REVERSED),
        (Flags::HIDDEN, Modifier::HIDDEN),
        (Flags::STRIKEOUT, Modifier::CROSSED_OUT),
    ] {
        if cell.flags.contains(flag) { modifiers.insert(modifier); }
    }
    let underline = if cell.flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineStyle::DoubleLine
    } else if cell.flags.contains(Flags::UNDERCURL) {
        UnderlineStyle::Curl
    } else if cell.flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if cell.flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else if cell.flags.contains(Flags::UNDERLINE) {
        UnderlineStyle::Line
    } else {
        UnderlineStyle::Reset
    };
    let mut style = Style::default()
        .fg(color(cell.fg, screen))
        .bg(color(cell.bg, screen))
        .add_modifier(modifiers)
        .underline_style(underline);
    if let Some(underline_color) = cell.underline_color() {
        style = style.underline_color(color(underline_color, screen));
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_geometry_stays_inside_viewport() {
        for height in 0..40 {
            let area = Rect { x: 3, y: 5, width: 80, height };
            let body = body_area(area);
            assert!(body.y >= area.y);
            assert_eq!(body.bottom(), area.bottom());
            assert_eq!(body.width, area.width);
            assert!(body.height < height || height == 0);
        }
    }

    #[test]
    fn renders_alacritty_attributes_and_palette_overrides() {
        let mut screen = Screen::new(2, 20);
        screen.process(b"\x1b]4;1;rgb:12/34/56\x07\x1b[31;1;9;4:3mX");
        let cell = screen.cell(0, 0).unwrap();
        let style = cell_style(cell, &screen);
        assert_eq!(style.fg, Some(Color::Rgb(0x12, 0x34, 0x56)));
        assert!(style.add_modifier.contains(Modifier::BOLD | Modifier::CROSSED_OUT));
        assert_eq!(style.underline_style, Some(UnderlineStyle::Curl));
    }
}
