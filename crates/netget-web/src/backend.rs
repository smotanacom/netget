//! A ratatui backend that speaks ANSI to xterm.js.
//!
//! The dashboard draws into a ratatui `Buffer`; ratatui diffs it against the previous frame
//! and hands the backend the cells that changed. This backend turns those cells into the
//! same escape sequences `CrosstermBackend` would write to a tty — cursor moves, SGR colour
//! and attribute changes, the glyphs — and on `flush` hands the bytes to a JS callback that
//! does `terminal.write(bytes)`. xterm.js is a full VT emulator, so what the visitor sees is
//! what a terminal would show, mouse reporting included.

use std::cell::Cell;
use std::fmt::Write as _;
use std::io;
use std::rc::Rc;

use js_sys::Function;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell as BufCell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use wasm_bindgen::JsValue;

pub struct WebBackend {
    /// Columns and rows, updated from JS on every xterm.js resize.
    size: Rc<Cell<(u16, u16)>>,
    /// The JS sink: `(bytes: Uint8Array) => void`.
    sink: Function,
    out: String,
    cursor: Position,
}

impl WebBackend {
    pub fn new(size: Rc<Cell<(u16, u16)>>, sink: Function) -> Self {
        let mut backend = WebBackend {
            size,
            sink,
            out: String::new(),
            cursor: Position::ORIGIN,
        };
        // Same terminal setup the native front does: no cursor, and mouse reporting on so
        // xterm.js sends SGR mouse sequences (button, drag, wheel) back through `onData`.
        backend
            .out
            .push_str("\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        backend
    }

    fn emit(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let bytes = js_sys::Uint8Array::from(self.out.as_bytes());
        self.out.clear();
        let _ = self.sink.call1(&JsValue::NULL, &bytes);
    }
}

fn fg_code(color: Color) -> String {
    match color {
        Color::Reset => "39".into(),
        Color::Black => "30".into(),
        Color::Red => "31".into(),
        Color::Green => "32".into(),
        Color::Yellow => "33".into(),
        Color::Blue => "34".into(),
        Color::Magenta => "35".into(),
        Color::Cyan => "36".into(),
        Color::Gray => "37".into(),
        Color::DarkGray => "90".into(),
        Color::LightRed => "91".into(),
        Color::LightGreen => "92".into(),
        Color::LightYellow => "93".into(),
        Color::LightBlue => "94".into(),
        Color::LightMagenta => "95".into(),
        Color::LightCyan => "96".into(),
        Color::White => "97".into(),
        Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        Color::Indexed(i) => format!("38;5;{i}"),
    }
}

fn bg_code(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
        Color::Indexed(i) => format!("48;5;{i}"),
        // The 16 named colours and Reset are the foreground code plus ten.
        other => {
            let fg: u8 = fg_code(other).parse().unwrap_or(39);
            (fg + 10).to_string()
        }
    }
}

fn modifier_codes(m: Modifier) -> Vec<&'static str> {
    let mut codes = Vec::new();
    if m.contains(Modifier::BOLD) {
        codes.push("1");
    }
    if m.contains(Modifier::DIM) {
        codes.push("2");
    }
    if m.contains(Modifier::ITALIC) {
        codes.push("3");
    }
    if m.contains(Modifier::UNDERLINED) {
        codes.push("4");
    }
    if m.contains(Modifier::SLOW_BLINK) {
        codes.push("5");
    }
    if m.contains(Modifier::RAPID_BLINK) {
        codes.push("6");
    }
    if m.contains(Modifier::REVERSED) {
        codes.push("7");
    }
    if m.contains(Modifier::HIDDEN) {
        codes.push("8");
    }
    if m.contains(Modifier::CROSSED_OUT) {
        codes.push("9");
    }
    codes
}

impl Backend for WebBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a BufCell)>,
    {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last: Option<(u16, u16)> = None;
        // Start every frame from a known attribute state.
        self.out.push_str("\x1b[0m");
        for (x, y, cell) in content {
            if last != Some((x.wrapping_sub(1), y)) {
                let _ = write!(self.out, "\x1b[{};{}H", y + 1, x + 1);
            }
            last = Some((x, y));
            if cell.modifier != modifier {
                // Attributes cannot be removed one by one portably; reset and re-apply,
                // which also clears the colours, so re-emit those too.
                self.out.push_str("\x1b[0m");
                for code in modifier_codes(cell.modifier) {
                    let _ = write!(self.out, "\x1b[{code}m");
                }
                modifier = cell.modifier;
                fg = Color::Reset;
                bg = Color::Reset;
            }
            if cell.fg != fg {
                let _ = write!(self.out, "\x1b[{}m", fg_code(cell.fg));
                fg = cell.fg;
            }
            if cell.bg != bg {
                let _ = write!(self.out, "\x1b[{}m", bg_code(cell.bg));
                bg = cell.bg;
            }
            self.out.push_str(cell.symbol());
        }
        self.out.push_str("\x1b[0m");
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.out.push_str("\x1b[?25l");
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.out.push_str("\x1b[?25h");
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let pos = position.into();
        self.cursor = pos;
        let _ = write!(self.out, "\x1b[{};{}H", pos.y + 1, pos.x + 1);
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.out.push_str("\x1b[2J\x1b[H");
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.out.push_str(match clear_type {
            ClearType::All => "\x1b[2J",
            ClearType::AfterCursor => "\x1b[J",
            ClearType::BeforeCursor => "\x1b[1J",
            ClearType::CurrentLine => "\x1b[2K",
            ClearType::UntilNewLine => "\x1b[K",
        });
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        let (cols, rows) = self.size.get();
        Ok(Size::new(cols.max(1), rows.max(1)))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size()?,
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit();
        Ok(())
    }
}
