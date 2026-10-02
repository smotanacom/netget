//! Syntax highlighting for script code in logs

use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEMES: OnceLock<ThemeSet> = OnceLock::new();

/// Highlight script code for terminal output
///
/// Uses ANSI escape codes to add syntax highlighting to code.
/// Falls back to plain text if highlighting fails.
pub fn highlight_code(code: &str, language: &str) -> String {
    // Load syntax definitions and theme
    let ps = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let ts = THEMES.get_or_init(ThemeSet::load_defaults);

    // Use a dark theme suitable for terminals
    let theme = &ts.themes["base16-ocean.dark"];

    // Find the syntax definition for the language
    let syntax = ps
        .find_syntax_by_token(language)
        .unwrap_or_else(|| ps.find_syntax_plain_text());

    // Highlight the code
    let mut h = HighlightLines::new(syntax, theme);
    let mut highlighted = String::new();

    for line in LinesWithEndings::from(code) {
        if let Ok(ranges) = h.highlight_line(line, ps) {
            let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
            highlighted.push_str(&escaped);
        } else {
            // Fallback to plain text if highlighting fails
            highlighted.push_str(line);
        }
    }

    // Add reset code at the end to prevent color bleeding
    highlighted.push_str("\x1b[0m");

    highlighted
}

/// Format script code with a header and syntax highlighting
///
/// Returns a formatted, highlighted code block suitable for DEBUG logs
pub fn format_script_for_log(code: &str, language: &str) -> String {
    let highlighted = highlight_code(code, language);

    format!(
        "\n┌─────────────────────────────────────────────┐\n\
         │ LLM-Generated {} Script                     \n\
         └─────────────────────────────────────────────┘\n\
         {}\n\
         ─────────────────────────────────────────────────",
        language.to_uppercase(),
        highlighted
    )
}
