//! Text rendering using cosmic-text

use crate::display::types::Color;
use cosmic_text::{fontdb, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::Pixmap;

pub const MAX_FONT_SIZE: u32 = 512;
const MAX_GLYPH_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Text renderer using cosmic-text for advanced text handling
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    cache_bytes: usize,
}

impl TextRenderer {
    /// Create a new text renderer with system fonts
    pub fn new() -> Self {
        let mut font_db = fontdb::Database::new();
        font_db.load_system_fonts();

        let font_system = FontSystem::new_with_locale_and_db(
            sys_locale::get_locale().unwrap_or_else(|| String::from("en-US")),
            font_db,
        );

        Self {
            font_system,
            swash_cache: SwashCache::new(),
            cache_bytes: 0,
        }
    }

    /// Draw text on a pixmap at the specified position
    pub fn draw_text(
        &mut self,
        pixmap: &mut Pixmap,
        x: u32,
        y: u32,
        text: &str,
        font_size: u32,
        color: Color,
    ) {
        self.draw_with_family(pixmap, x, y, text, font_size, color, Family::SansSerif);
    }

    /// Render fixed-width text, for ASCII art whose columns must align.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_with_family(
        &mut self,
        pixmap: &mut Pixmap,
        x: u32,
        y: u32,
        text: &str,
        font_size: u32,
        color: Color,
        family: Family<'_>,
    ) {
        if font_size == 0
            || font_size > MAX_FONT_SIZE
            || text.len() > super::canvas::MAX_CANVAS_TEXT_BYTES
            || text.is_empty()
            || color.a == 0
            || x >= pixmap.width()
            || y >= pixmap.height()
        {
            return;
        }
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(font_size as f32, font_size as f32),
        );
        buffer.set_wrap(&mut self.font_system, cosmic_text::Wrap::None);
        buffer.set_size(
            &mut self.font_system,
            Some((pixmap.width() - x) as f32),
            Some((pixmap.height() - y) as f32),
        );
        buffer.set_text(
            &mut self.font_system,
            text,
            Attrs::new().family(family),
            Shaping::Advanced,
        );

        // Rasterize visible glyphs only, retaining cosmic-text's baseline and
        // RGBA/mask handling. Bound the shared cache between glyphs.
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                if glyph.x >= (pixmap.width() - x) as f32 || glyph.x + glyph.w < 0.0 {
                    continue;
                }
                if self.cache_bytes > MAX_GLYPH_CACHE_BYTES {
                    self.swash_cache.image_cache.clear();
                    self.swash_cache.outline_command_cache.clear();
                    self.cache_bytes = 0;
                }
                let physical = glyph.physical((0.0, 0.0), 1.0);
                let cached = self
                    .swash_cache
                    .image_cache
                    .contains_key(&physical.cache_key);
                let base = glyph
                    .color_opt
                    .unwrap_or_else(|| cosmic_text::Color::rgb(color.r, color.g, color.b));
                self.swash_cache.with_pixels(
                    &mut self.font_system,
                    physical.cache_key,
                    base,
                    |glyph_x, glyph_y, pixel| {
                        let px = i64::from(x) + i64::from(physical.x) + i64::from(glyph_x);
                        let py = i64::from(y)
                            + i64::from(run.line_y as i32)
                            + i64::from(physical.y)
                            + i64::from(glyph_y);
                        if px < 0
                            || py < 0
                            || px >= i64::from(pixmap.width())
                            || py >= i64::from(pixmap.height())
                        {
                            return;
                        }
                        let index = py as usize * pixmap.width() as usize + px as usize;
                        let existing = pixmap.pixels()[index];
                        let scale =
                            |v: u8, a: u8| ((u32::from(v) * u32::from(a) + 127) / 255) as u8;
                        let alpha = scale(pixel.a(), color.a);
                        let inverse = 255 - alpha;
                        let blended = tiny_skia::PremultipliedColorU8::from_rgba(
                            scale(pixel.r(), alpha) + scale(existing.red(), inverse),
                            scale(pixel.g(), alpha) + scale(existing.green(), inverse),
                            scale(pixel.b(), alpha) + scale(existing.blue(), inverse),
                            alpha + scale(existing.alpha(), inverse),
                        );
                        if let Some(blended) = blended {
                            pixmap.pixels_mut()[index] = blended;
                        }
                    },
                );
                if !cached {
                    self.cache_bytes = self.cache_bytes.saturating_add(
                        self.swash_cache
                            .image_cache
                            .get(&physical.cache_key)
                            .and_then(Option::as_ref)
                            .map_or(0, |image| image.data.len()),
                    );
                }
            }
        }
    }
}

impl Default for TextRenderer {
    fn default() -> Self {
        Self::new()
    }
}
