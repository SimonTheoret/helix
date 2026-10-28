//! A persistent terminal layer. Hiding it preserves the shell and scrollback.
use std::sync::Arc;

use helix_core::Position;
use helix_view::graphics::{Color, CursorKind, Modifier, Rect, Style, UnderlineStyle};
use helix_view::input::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use helix_view::Editor;
use tui::buffer::Buffer;

use crate::compositor::{Component, Context, Event, EventResult};
use crate::terminal::{encode_key, paste, Session};

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
        self.session.parser.screen_mut().set_scrollback(0);
        if let Err(err) = self.session.send(bytes) {
            ctx.editor.set_error(err.to_string());
        }
    }

    fn scroll(&mut self, up: bool, amount: usize) {
        let screen = self.session.parser.screen_mut();
        if screen.alternate_screen() {
            return;
        }
        let offset = screen.scrollback();
        screen.set_scrollback(if up {
            offset.saturating_add(amount)
        } else {
            offset.saturating_sub(amount)
        });
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
        let bytes = encode_key(key, self.session.parser.screen().application_cursor());
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
                let bracketed = self.session.parser.screen().bracketed_paste();
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

        let screen = self.session.parser.screen();
        for row in 0..body.height {
            for col in 0..body.width {
                let target = frame.get_mut(body.x + col, body.y + row);
                target.reset();
                if let Some(source) = screen.cell(row, col) {
                    // The renderer accounts for wide glyphs; do not draw their
                    // continuation cells as independent glyphs.
                    if !source.is_wide_continuation() {
                        let contents = source.contents();
                        target.set_symbol(if contents.is_empty() { " " } else { contents });
                    }
                    target.set_style(cell_style(source));
                }
            }
        }
    }

    fn cursor(&self, area: Rect, _editor: &Editor) -> (Option<Position>, CursorKind) {
        if !self.visible {
            return (None, CursorKind::Hidden);
        }
        let body = body_area(area);
        let screen = self.session.parser.screen();
        let (row, col) = screen.cursor_position();
        let hidden = screen.hide_cursor() || screen.scrollback() != 0 || body.height == 0
            || self.session.exit.is_some();
        let position = Position::new(
            (body.y + row.min(body.height.saturating_sub(1)))
                .min(area.bottom().saturating_sub(1)).max(area.y) as usize,
            (body.x + col.min(body.width.saturating_sub(1)))
                .min(area.right().saturating_sub(1)).max(area.x) as usize,
        );
        // Some(position) also prevents the underlying editor cursor showing
        // through when an application hides its own cursor.
        (Some(position), if hidden { CursorKind::Hidden } else { CursorKind::Block })
    }
}

fn body_area(area: Rect) -> Rect {
    let height = area.height.saturating_sub(1) / 2;
    Rect { x: area.x, y: area.bottom() - height, width: area.width, height }
}

fn color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut modifiers = Modifier::empty();
    for (enabled, modifier) in [
        (cell.bold(), Modifier::BOLD),
        (cell.dim(), Modifier::DIM),
        (cell.italic(), Modifier::ITALIC),
        (cell.inverse(), Modifier::REVERSED),
    ] {
        if enabled { modifiers.insert(modifier); }
    }
    Style::default()
        .fg(color(cell.fgcolor()))
        .bg(color(cell.bgcolor()))
        .add_modifier(modifiers)
        .underline_style(if cell.underline() { UnderlineStyle::Line } else { UnderlineStyle::Reset })
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
}
