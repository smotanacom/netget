//! DOM input to crossterm events.
//!
//! The page hands keys over as the fields of a `KeyboardEvent` (`key`, `ctrlKey`, `altKey`,
//! `shiftKey`), and mouse reports as xterm.js's SGR sequences already split into their parts.
//! Both become the `crossterm::event::Event` the dashboard's key handlers match on.

use netget_crossterm_wasm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct KeyInput {
    pub key: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub meta: bool,
}

#[derive(Deserialize)]
pub struct MouseInput {
    /// "down" | "up" | "drag" | "moved" | "scrollup" | "scrolldown"
    pub kind: String,
    /// "left" | "right" | "middle"
    #[serde(default)]
    pub button: Option<String>,
    pub col: u16,
    pub row: u16,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
}

fn modifiers(ctrl: bool, alt: bool, shift: bool, meta: bool) -> KeyModifiers {
    let mut m = KeyModifiers::empty();
    if ctrl {
        m |= KeyModifiers::CONTROL;
    }
    if alt {
        m |= KeyModifiers::ALT;
    }
    if shift {
        m |= KeyModifiers::SHIFT;
    }
    if meta {
        m |= KeyModifiers::SUPER;
    }
    m
}

/// A `KeyboardEvent` as a key press, or `None` for a key the dashboard has no use for (a
/// modifier on its own, dead keys, IME composition).
pub fn key_event(input: &KeyInput) -> Option<Event> {
    let code = match input.key.as_str() {
        "Enter" => KeyCode::Enter,
        "Backspace" => KeyCode::Backspace,
        "Tab" => {
            if input.shift {
                KeyCode::BackTab
            } else {
                KeyCode::Tab
            }
        }
        "Escape" => KeyCode::Esc,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        " " | "Spacebar" => KeyCode::Char(' '),
        k if k.len() > 1 && k.starts_with('F') => {
            let n: u8 = k[1..].parse().ok()?;
            KeyCode::F(n)
        }
        k => {
            let mut chars = k.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                // "Shift", "Control", "Dead", "Process", ... — nothing to type.
                return None;
            }
            KeyCode::Char(c)
        }
    };
    // Shift is already expressed in the character for printable keys, as crossterm does on
    // a tty; for BackTab it is folded into the code above.
    let shift = input.shift && !matches!(code, KeyCode::Char(_) | KeyCode::BackTab);
    Some(Event::Key(KeyEvent {
        code,
        modifiers: modifiers(input.ctrl, input.alt, shift, input.meta),
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    }))
}

/// A mouse report as the dashboard's `MouseEvent`.
pub fn mouse_event(input: &MouseInput) -> Option<Event> {
    let button = match input.button.as_deref() {
        Some("right") => MouseButton::Right,
        Some("middle") => MouseButton::Middle,
        _ => MouseButton::Left,
    };
    let kind = match input.kind.as_str() {
        "down" => MouseEventKind::Down(button),
        "up" => MouseEventKind::Up(button),
        "drag" => MouseEventKind::Drag(button),
        "moved" => MouseEventKind::Moved,
        "scrollup" => MouseEventKind::ScrollUp,
        "scrolldown" => MouseEventKind::ScrollDown,
        _ => return None,
    };
    Some(Event::Mouse(MouseEvent {
        kind,
        column: input.col,
        row: input.row,
        modifiers: modifiers(input.ctrl, input.alt, input.shift, false),
    }))
}

/// Text typed or pasted straight into the terminal (xterm.js `onData` that is not a mouse
/// report): one key press per character, newline as Enter.
pub fn text_events(text: &str) -> Vec<Event> {
    let mut events = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let code = match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                KeyCode::Enter
            }
            '\n' => KeyCode::Enter,
            '\t' => KeyCode::Tab,
            '\x7f' | '\x08' => KeyCode::Backspace,
            '\x1b' => KeyCode::Esc,
            c if c.is_control() => continue,
            c => KeyCode::Char(c),
        };
        events.push(Event::Key(KeyEvent::new(code, KeyModifiers::empty())));
    }
    events
}
