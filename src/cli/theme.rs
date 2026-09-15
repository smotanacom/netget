use crossterm::style::Color;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
use tracing::debug;

/// Theme variants based on terminal background
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
    Neutral,
}

/// Color palette for TUI with semantic color names
#[derive(Debug, Clone)]
pub struct ColorPalette {
    // Log level colors
    pub error: Color,
    pub warning: Color,
    pub info: Color,
    pub debug: Color,
    pub trace: Color,
    /// Streamed model reasoning/chain-of-thought (`[REASONING]` lines).
    pub reasoning: Color,
    pub user: Color,

    // Connection/server/client indicators
    pub server: Color,
    pub client: Color,
    pub connection: Color,

    // UI elements
    pub separator: Color,
    pub dimmed: Color,
    pub normal: Color,

    // Status indicators
    pub success: Color,
    pub failure: Color,
    pub ask: Color,
}

impl ColorPalette {
    /// Create a color palette for dark terminals (current NetGet default)
    pub fn dark() -> Self {
        Self {
            error: Color::Red,
            warning: Color::Yellow,
            // NOT ansi Blue: on a dark background that is the classic
            // unreadable pair, and `info` is the dashboard's accent — field
            // values, focused borders, buttons. 75 is a bright steel blue
            // that stays legible on black (256-color, which every TERM the
            // dashboard runs under supports).
            info: Color::AnsiValue(75),
            debug: Color::Cyan,
            trace: Color::DarkGrey,
            reasoning: Color::Magenta,
            user: Color::Green,
            server: Color::Cyan,
            client: Color::Magenta,
            connection: Color::Cyan,
            separator: Color::DarkGreen,
            dimmed: Color::DarkGrey,
            normal: Color::White,
            success: Color::Green,
            failure: Color::Red,
            ask: Color::Yellow,
        }
    }

    /// Create a color palette for light terminals
    pub fn light() -> Self {
        Self {
            error: Color::DarkRed,
            warning: Color::DarkYellow,
            info: Color::DarkBlue,
            debug: Color::DarkCyan,
            trace: Color::DarkGrey,
            reasoning: Color::DarkMagenta,
            user: Color::DarkGreen,
            server: Color::DarkCyan,
            client: Color::DarkMagenta,
            connection: Color::DarkCyan,
            separator: Color::DarkGreen,
            dimmed: Color::Grey,
            normal: Color::Black,
            success: Color::DarkGreen,
            failure: Color::DarkRed,
            ask: Color::DarkYellow,
        }
    }

    /// Create a neutral color palette that works on both light and dark backgrounds
    /// Uses medium contrast colors that are readable in most situations
    pub fn neutral() -> Self {
        Self {
            error: Color::Red,
            warning: Color::DarkYellow,
            // A mid-brightness 256-color blue that reads on both black and
            // white; ansi Blue is near-invisible on dark terminals, and
            // neutral is exactly the palette a dark terminal lands on when
            // detection fails.
            info: Color::AnsiValue(32),
            debug: Color::DarkCyan,
            trace: Color::Grey,
            reasoning: Color::Magenta,
            user: Color::DarkGreen,
            server: Color::DarkCyan,
            client: Color::DarkMagenta,
            connection: Color::DarkCyan,
            separator: Color::DarkGreen,
            dimmed: Color::Grey,
            normal: Color::Reset, // Use terminal default
            success: Color::DarkGreen,
            failure: Color::Red,
            ask: Color::DarkYellow,
        }
    }

    /// Get the appropriate color palette based on theme
    pub fn from_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self::dark(),
            Theme::Light => Self::light(),
            Theme::Neutral => Self::neutral(),
        }
    }
}

/// Detect terminal background and determine appropriate theme
/// Returns None if detection fails or times out
///
/// Note: termbg can leave the terminal in a bad state on some terminals
/// (especially macOS Terminal.app). We wrap detection in catch_unwind and
/// flush any stale input afterwards.
#[cfg(not(target_arch = "wasm32"))]
pub fn detect_theme() -> Option<Theme> {
    use std::panic::catch_unwind;

    // Check for known-problematic terminal environments
    // macOS Terminal.app and some other terminals don't handle OSC queries well
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        // Apple Terminal is known to have issues with termbg
        if term_program == "Apple_Terminal" {
            debug!("Skipping theme detection on Apple Terminal (known issues)");
            return theme_from_colorfgbg();
        }
    }

    // Use a short timeout to avoid blocking startup
    let timeout = Duration::from_millis(100);

    // Wrap in catch_unwind to handle any panics from termbg
    let result = catch_unwind(|| termbg::theme(timeout));

    // Flush any leftover input that termbg might have left in the buffer
    // This is critical - termbg sends OSC sequences and if the terminal
    // doesn't respond or responds incorrectly, garbage can be left in stdin
    flush_stdin_nonblocking();

    match result {
        Ok(Ok(termbg::Theme::Light)) => Some(Theme::Light),
        Ok(Ok(termbg::Theme::Dark)) => Some(Theme::Dark),
        Ok(Err(e)) => {
            debug!("Theme detection failed: {:?}", e);
            theme_from_colorfgbg()
        }
        Err(_) => {
            debug!("Theme detection panicked");
            theme_from_colorfgbg()
        }
    }
}

/// Second-chance detection from `$COLORFGBG` ("fg;bg", e.g. "15;0").
///
/// Terminals that don't answer the OSC query (or where we dare not send it,
/// like Apple Terminal) often still export this. Falling back here is what
/// keeps a dark terminal from landing on the neutral palette, whose blue is
/// barely readable on black — the exact complaint this fixes.
#[cfg(not(target_arch = "wasm32"))]
fn theme_from_colorfgbg() -> Option<Theme> {
    let value = std::env::var("COLORFGBG").ok()?;
    let bg = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    // ANSI 0-6 and 8 are dark backgrounds; 7 and 15 are light.
    let theme = if bg == 7 || bg == 15 {
        Theme::Light
    } else {
        Theme::Dark
    };
    debug!("Theme from COLORFGBG={value}: {theme:?}");
    Some(theme)
}

/// Flush any pending input from stdin without blocking
/// This cleans up any stale escape sequences that termbg may have left
#[cfg(not(target_arch = "wasm32"))]
fn flush_stdin_nonblocking() {
    use std::io::Read;

    // On Unix, we can set stdin to non-blocking temporarily
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();

        // Get current flags
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return;
        }

        // Set non-blocking
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return;
        }

        // Read and discard any pending data
        let mut buf = [0u8; 256];
        let mut stdin_lock = stdin.lock();
        while stdin_lock.read(&mut buf).unwrap_or(0) > 0 {}

        // Restore original flags
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    }

    // On non-Unix platforms, we just do nothing - they typically don't have
    // the same issues with termbg
    #[cfg(not(unix))]
    {}
}

/// There is no terminal to query in the browser; the page passes its theme in explicitly.
#[cfg(target_arch = "wasm32")]
pub fn detect_theme() -> Option<Theme> {
    None
}

/// Parse theme from string (for CLI flag)
pub fn parse_theme(s: &str) -> anyhow::Result<Option<Theme>> {
    match s.to_lowercase().as_str() {
        "auto" => Ok(None), // None means auto-detect
        "light" => Ok(Some(Theme::Light)),
        "dark" => Ok(Some(Theme::Dark)),
        "neutral" => Ok(Some(Theme::Neutral)),
        _ => anyhow::bail!(
            "Invalid theme '{}'. Valid options: auto, light, dark, neutral",
            s
        ),
    }
}
