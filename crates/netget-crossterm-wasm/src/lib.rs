//! `crossterm` for the browser: the data types, none of the terminal.
//!
//! NetGet's dashboard (`src/tui/`) consumes `crossterm::event::KeyEvent` and friends in its
//! key handlers and carries `crossterm::style::Color` in its palette. crossterm itself does not
//! compile for `wasm32-unknown-unknown` (it needs a tty), so on that target the root
//! `Cargo.toml` points the name `crossterm` at this crate. The enums here are field-for-field
//! copies of crossterm 0.28's, so every `match key.code { KeyCode::Char('q') => .. }` compiles
//! unchanged; what changes is where a `KeyEvent` comes from — the web crate builds one from a
//! DOM `KeyboardEvent`.
//!
//! Two conversions the real crates provide behind their `crossterm` features are provided
//! here instead, so the dashboard's `Color::from(palette_color)` and
//! `tui_textarea::Input::from(key)` need no `#[cfg]`.

pub mod event {
    use bitflags::bitflags;
    use std::fmt;

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Hash)]
    pub enum Event {
        FocusGained,
        FocusLost,
        Key(KeyEvent),
        Mouse(MouseEvent),
        Paste(String),
        Resize(u16, u16),
    }

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
    pub struct MouseEvent {
        pub kind: MouseEventKind,
        pub column: u16,
        pub row: u16,
        pub modifiers: KeyModifiers,
    }

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
    pub enum MouseEventKind {
        Down(MouseButton),
        Up(MouseButton),
        Drag(MouseButton),
        Moved,
        ScrollDown,
        ScrollUp,
        ScrollLeft,
        ScrollRight,
    }

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
    pub enum MouseButton {
        Left,
        Right,
        Middle,
    }

    bitflags! {
        #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
        pub struct KeyModifiers: u8 {
            const SHIFT = 0b0000_0001;
            const CONTROL = 0b0000_0010;
            const ALT = 0b0000_0100;
            const SUPER = 0b0000_1000;
            const HYPER = 0b0001_0000;
            const META = 0b0010_0000;
            const NONE = 0b0000_0000;
        }
    }

    impl fmt::Display for KeyModifiers {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let mut first = true;
            for modifier in self.iter() {
                if !first {
                    f.write_str("+")?;
                }
                first = false;
                match modifier {
                    KeyModifiers::SHIFT => f.write_str("Shift")?,
                    KeyModifiers::CONTROL => f.write_str("Ctrl")?,
                    KeyModifiers::ALT => f.write_str("Alt")?,
                    KeyModifiers::SUPER => f.write_str("Super")?,
                    KeyModifiers::HYPER => f.write_str("Hyper")?,
                    KeyModifiers::META => f.write_str("Meta")?,
                    _ => {}
                }
            }
            Ok(())
        }
    }

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
    pub enum KeyEventKind {
        Press,
        Repeat,
        Release,
    }

    bitflags! {
        #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
        pub struct KeyEventState: u8 {
            const KEYPAD = 0b0000_0001;
            const CAPS_LOCK = 0b0000_0010;
            const NUM_LOCK = 0b0000_0100;
            const NONE = 0b0000_0000;
        }
    }

    #[derive(Debug, PartialOrd, Clone, Copy)]
    pub struct KeyEvent {
        pub code: KeyCode,
        pub modifiers: KeyModifiers,
        pub kind: KeyEventKind,
        pub state: KeyEventState,
    }

    impl KeyEvent {
        pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
            KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            }
        }

        pub const fn new_with_kind(
            code: KeyCode,
            modifiers: KeyModifiers,
            kind: KeyEventKind,
        ) -> KeyEvent {
            KeyEvent {
                code,
                modifiers,
                kind,
                state: KeyEventState::empty(),
            }
        }

        pub const fn new_with_kind_and_state(
            code: KeyCode,
            modifiers: KeyModifiers,
            kind: KeyEventKind,
            state: KeyEventState,
        ) -> KeyEvent {
            KeyEvent {
                code,
                modifiers,
                kind,
                state,
            }
        }

        /// SHIFT is present iff the character is uppercase, as crossterm normalises.
        fn normalize_case(mut self) -> KeyEvent {
            let c = match self.code {
                KeyCode::Char(c) => c,
                _ => return self,
            };
            if c.is_ascii_uppercase() {
                self.modifiers.insert(KeyModifiers::SHIFT);
            } else if self.modifiers.contains(KeyModifiers::SHIFT) {
                self.code = KeyCode::Char(c.to_ascii_uppercase())
            }
            self
        }
    }

    impl From<KeyCode> for KeyEvent {
        fn from(code: KeyCode) -> Self {
            KeyEvent::new(code, KeyModifiers::empty())
        }
    }

    impl PartialEq for KeyEvent {
        fn eq(&self, other: &KeyEvent) -> bool {
            let a = self.normalize_case();
            let b = other.normalize_case();
            a.code == b.code && a.modifiers == b.modifiers && a.kind == b.kind && a.state == b.state
        }
    }

    impl Eq for KeyEvent {}

    impl std::hash::Hash for KeyEvent {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            let k = self.normalize_case();
            k.code.hash(state);
            k.modifiers.hash(state);
            k.kind.hash(state);
            k.state.hash(state);
        }
    }

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
    pub enum MediaKeyCode {
        Play,
        Pause,
        PlayPause,
        Reverse,
        Stop,
        FastForward,
        Rewind,
        TrackNext,
        TrackPrevious,
        Record,
        LowerVolume,
        RaiseVolume,
        MuteVolume,
    }

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
    pub enum ModifierKeyCode {
        LeftShift,
        LeftControl,
        LeftAlt,
        LeftSuper,
        LeftHyper,
        LeftMeta,
        RightShift,
        RightControl,
        RightAlt,
        RightSuper,
        RightHyper,
        RightMeta,
        IsoLevel3Shift,
        IsoLevel5Shift,
    }

    #[derive(Debug, PartialOrd, PartialEq, Eq, Clone, Copy, Hash)]
    pub enum KeyCode {
        Backspace,
        Enter,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        PageUp,
        PageDown,
        Tab,
        BackTab,
        Delete,
        Insert,
        F(u8),
        Char(char),
        Null,
        Esc,
        CapsLock,
        ScrollLock,
        NumLock,
        PrintScreen,
        Pause,
        Menu,
        KeypadBegin,
        Media(MediaKeyCode),
        Modifier(ModifierKeyCode),
    }

    impl fmt::Display for KeyCode {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                KeyCode::Backspace => write!(f, "Backspace"),
                KeyCode::Enter => write!(f, "Enter"),
                KeyCode::Left => write!(f, "Left"),
                KeyCode::Right => write!(f, "Right"),
                KeyCode::Up => write!(f, "Up"),
                KeyCode::Down => write!(f, "Down"),
                KeyCode::Home => write!(f, "Home"),
                KeyCode::End => write!(f, "End"),
                KeyCode::PageUp => write!(f, "Page Up"),
                KeyCode::PageDown => write!(f, "Page Down"),
                KeyCode::Tab => write!(f, "Tab"),
                KeyCode::BackTab => write!(f, "Back Tab"),
                KeyCode::Delete => write!(f, "Del"),
                KeyCode::Insert => write!(f, "Insert"),
                KeyCode::F(n) => write!(f, "F{n}"),
                KeyCode::Char(' ') => write!(f, "Space"),
                KeyCode::Char(c) => write!(f, "{c}"),
                KeyCode::Null => write!(f, "Null"),
                KeyCode::Esc => write!(f, "Esc"),
                KeyCode::CapsLock => write!(f, "Caps Lock"),
                KeyCode::ScrollLock => write!(f, "Scroll Lock"),
                KeyCode::NumLock => write!(f, "Num Lock"),
                KeyCode::PrintScreen => write!(f, "Print Screen"),
                KeyCode::Pause => write!(f, "Pause"),
                KeyCode::Menu => write!(f, "Menu"),
                KeyCode::KeypadBegin => write!(f, "Begin"),
                KeyCode::Media(m) => write!(f, "{m:?}"),
                KeyCode::Modifier(m) => write!(f, "{m:?}"),
            }
        }
    }

    /// What `tui-textarea`'s `crossterm` feature provides natively, so
    /// `editor.textarea.input(Input::from(key))` compiles here too.
    impl From<KeyEvent> for tui_textarea::Input {
        fn from(key: KeyEvent) -> Self {
            use tui_textarea::Key;
            if key.kind == KeyEventKind::Release {
                return Self::default();
            }
            let k = match key.code {
                KeyCode::Char(c) => Key::Char(c),
                KeyCode::Backspace => Key::Backspace,
                KeyCode::Enter => Key::Enter,
                KeyCode::Left => Key::Left,
                KeyCode::Right => Key::Right,
                KeyCode::Up => Key::Up,
                KeyCode::Down => Key::Down,
                KeyCode::Tab => Key::Tab,
                KeyCode::Delete => Key::Delete,
                KeyCode::Home => Key::Home,
                KeyCode::End => Key::End,
                KeyCode::PageUp => Key::PageUp,
                KeyCode::PageDown => Key::PageDown,
                KeyCode::Esc => Key::Esc,
                KeyCode::F(x) => Key::F(x),
                _ => Key::Null,
            };
            Self {
                key: k,
                ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
                alt: key.modifiers.contains(KeyModifiers::ALT),
                shift: key.modifiers.contains(KeyModifiers::SHIFT),
            }
        }
    }

    impl From<Event> for tui_textarea::Input {
        fn from(event: Event) -> Self {
            match event {
                Event::Key(key) => Self::from(key),
                Event::Mouse(mouse) => {
                    use tui_textarea::Key;
                    let key = match mouse.kind {
                        MouseEventKind::ScrollDown => Key::MouseScrollDown,
                        MouseEventKind::ScrollUp => Key::MouseScrollUp,
                        _ => Key::Null,
                    };
                    Self {
                        key,
                        ctrl: mouse.modifiers.contains(KeyModifiers::CONTROL),
                        alt: mouse.modifiers.contains(KeyModifiers::ALT),
                        shift: mouse.modifiers.contains(KeyModifiers::SHIFT),
                    }
                }
                _ => Self::default(),
            }
        }
    }
}

pub mod style {
    /// crossterm 0.28's `Color`, variant for variant.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub enum Color {
        Reset,
        Black,
        DarkGrey,
        Red,
        DarkRed,
        Green,
        DarkGreen,
        Yellow,
        DarkYellow,
        Blue,
        DarkBlue,
        Magenta,
        DarkMagenta,
        Cyan,
        DarkCyan,
        White,
        Grey,
        Rgb { r: u8, g: u8, b: u8 },
        AnsiValue(u8),
    }

    /// What ratatui's `crossterm` feature provides natively.
    impl From<Color> for ratatui::style::Color {
        fn from(c: Color) -> Self {
            use ratatui::style::Color as R;
            match c {
                Color::Reset => R::Reset,
                Color::Black => R::Black,
                Color::DarkGrey => R::DarkGray,
                Color::Red => R::LightRed,
                Color::DarkRed => R::Red,
                Color::Green => R::LightGreen,
                Color::DarkGreen => R::Green,
                Color::Yellow => R::LightYellow,
                Color::DarkYellow => R::Yellow,
                Color::Blue => R::LightBlue,
                Color::DarkBlue => R::Blue,
                Color::Magenta => R::LightMagenta,
                Color::DarkMagenta => R::Magenta,
                Color::Cyan => R::LightCyan,
                Color::DarkCyan => R::Cyan,
                Color::White => R::White,
                Color::Grey => R::Gray,
                Color::Rgb { r, g, b } => R::Rgb(r, g, b),
                Color::AnsiValue(v) => R::Indexed(v),
            }
        }
    }
}
