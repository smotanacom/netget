//! ASCII art renderer using monospace fonts

use crate::display::text::TextRenderer;
use crate::display::types::Color;
use tiny_skia::Pixmap;

/// ASCII art renderer for monospace text rendering
pub struct AsciiRenderer {
    text_renderer: TextRenderer,
}

impl AsciiRenderer {
    /// Create a new ASCII art renderer
    pub fn new() -> Self {
        Self {
            text_renderer: TextRenderer::new(),
        }
    }

    /// Render ASCII art to a pixmap
    ///
    /// The text is rendered with a monospace font starting at position (0, 0).
    /// Background color fills the entire pixmap before rendering text.
    pub fn render(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        font_size: u32,
        fg_color: Color,
        bg_color: Color,
    ) {
        Self::render_with(
            &mut self.text_renderer,
            pixmap,
            text,
            font_size,
            fg_color,
            bg_color,
        );
    }

    pub(crate) fn render_with(
        text_renderer: &mut TextRenderer,
        pixmap: &mut Pixmap,
        text: &str,
        font_size: u32,
        fg_color: Color,
        bg_color: Color,
    ) {
        // Fill background
        let sk_bg = tiny_skia::Color::from_rgba8(bg_color.r, bg_color.g, bg_color.b, bg_color.a);
        pixmap.fill(sk_bg);

        if font_size == 0 {
            return;
        }

        // Render each line of ASCII art
        let line_height = (font_size as f32 * 1.2) as u32;
        for (line_num, line) in text.lines().enumerate() {
            let y = 10u32.saturating_add(
                u32::try_from(line_num)
                    .unwrap_or(u32::MAX)
                    .saturating_mul(line_height),
            );
            if y >= pixmap.height() {
                break;
            }
            text_renderer.draw_with_family(
                pixmap,
                10,
                y,
                line,
                font_size,
                fg_color,
                cosmic_text::Family::Monospace,
            );
        }
    }
}

impl Default for AsciiRenderer {
    fn default() -> Self {
        Self::new()
    }
}
