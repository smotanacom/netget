//! Canvas implementation using tiny-skia for 2D graphics rendering

use crate::display::ascii::AsciiRenderer;
use crate::display::text::TextRenderer;
use crate::display::types::{Color, DisplayCommand};
use image::{ImageBuffer, Rgb};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Maximum retained pixel surface (RGBA scratch plus RGB output).
pub const MAX_CANVAS_PIXELS: u64 = 16 * 1024 * 1024;
pub const MAX_CANVAS_COMMANDS: usize = 10_000;
pub const MAX_CANVAS_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasError(pub &'static str);
impl std::fmt::Display for CanvasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for CanvasError {}

/// Display canvas that accumulates drawing commands and renders to an image buffer
pub struct DisplayCanvas {
    width: u32,
    height: u32,
    commands: Vec<DisplayCommand>,
}

impl DisplayCanvas {
    /// Create a new canvas with the specified dimensions
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            commands: Vec::new(),
        }
    }

    /// Add a drawing command to the canvas
    pub fn add_command(&mut self, cmd: DisplayCommand) {
        self.commands.push(cmd);
    }

    /// Add multiple drawing commands to the canvas
    pub fn add_commands(&mut self, cmds: Vec<DisplayCommand>) {
        self.commands.extend(cmds);
    }

    /// Clear all drawing commands
    pub fn clear_commands(&mut self) {
        drop_commands(std::mem::take(&mut self.commands));
    }

    /// Compatibility entry point. An invalid/oversized canvas renders an empty
    /// image; callers that need the reason should use `try_render`.
    pub fn render(&self) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        self.try_render().unwrap_or_else(|_| ImageBuffer::new(0, 0))
    }

    /// Bounded, fallible software rendering. Nested windows are visited without
    /// recursion or cloning; one font/glyph cache serves the entire render.
    pub fn try_render(&self) -> Result<ImageBuffer<Rgb<u8>, Vec<u8>>, CanvasError> {
        if u64::from(self.width) * u64::from(self.height) > MAX_CANVAS_PIXELS {
            return Err(CanvasError("canvas exceeds the 16-megapixel limit"));
        }
        if self.commands.len() > MAX_CANVAS_COMMANDS {
            return Err(CanvasError("canvas command limit exceeded"));
        }
        let mut pending = self
            .commands
            .iter()
            .rev()
            .map(|command| (command, 0u32, 0u32))
            .collect::<Vec<_>>();
        if pending.len() > MAX_CANVAS_COMMANDS {
            return Err(CanvasError("canvas command limit exceeded"));
        }
        let mut commands = Vec::new();
        let mut text_bytes = 0usize;
        while let Some((command, ox, oy)) = pending.pop() {
            if matches!(command, DisplayCommand::DrawText { font_size, .. } | DisplayCommand::RenderAsciiArt { font_size, .. } if *font_size > super::text::MAX_FONT_SIZE)
            {
                return Err(CanvasError("canvas font size exceeds 512 pixels"));
            }
            let text = match command {
                DisplayCommand::DrawText { text, .. }
                | DisplayCommand::RenderAsciiArt { text, .. } => text.len(),
                DisplayCommand::DrawButton { label, .. } => label.len(),
                DisplayCommand::DrawTextBox {
                    text, placeholder, ..
                } => text
                    .len()
                    .saturating_add(placeholder.as_ref().map_or(0, String::len)),
                DisplayCommand::DrawWindow {
                    x,
                    y,
                    title,
                    content,
                    ..
                } => {
                    if content.len()
                        > MAX_CANVAS_COMMANDS.saturating_sub(commands.len() + pending.len() + 1)
                    {
                        return Err(CanvasError("canvas command limit exceeded"));
                    }
                    pending.extend(content.iter().rev().map(|child| {
                        (
                            child,
                            ox.saturating_add(*x),
                            oy.saturating_add(*y).saturating_add(30),
                        )
                    }));
                    title.len()
                }
                _ => 0,
            };
            text_bytes = text_bytes.saturating_add(text);
            if text_bytes > MAX_CANVAS_TEXT_BYTES {
                return Err(CanvasError("canvas text limit exceeded"));
            }
            commands.push((command, ox, oy));
            if commands.len() > MAX_CANVAS_COMMANDS {
                return Err(CanvasError("canvas command limit exceeded"));
            }
        }
        if self.width == 0 || self.height == 0 {
            return Ok(ImageBuffer::new(self.width, self.height));
        }
        let size = tiny_skia::IntSize::from_wh(self.width, self.height)
            .ok_or(CanvasError("invalid canvas dimensions"))?;
        let rgba_len = self.width as usize * self.height as usize * 4;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(rgba_len)
            .map_err(|_| CanvasError("canvas allocation failed"))?;
        rgba.resize(rgba_len, 0);
        let mut pixmap =
            Pixmap::from_vec(rgba, size).ok_or(CanvasError("invalid pixel surface"))?;
        let mut renderer = None;
        for (command, ox, oy) in commands {
            self.execute_command(&mut pixmap, command, ox, oy, &mut renderer);
        }
        pixmap_to_image_buffer(&pixmap)
    }

    fn execute_command(
        &self,
        pixmap: &mut Pixmap,
        cmd: &DisplayCommand,
        ox: u32,
        oy: u32,
        renderer: &mut Option<TextRenderer>,
    ) {
        match cmd {
            DisplayCommand::SetBackground { color } => self.set_background(pixmap, *color),
            DisplayCommand::Clear => pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255)),
            DisplayCommand::DrawRectangle {
                x,
                y,
                width,
                height,
                color,
                filled,
            } => self.draw_rectangle(
                pixmap,
                x.saturating_add(ox),
                y.saturating_add(oy),
                *width,
                *height,
                *color,
                *filled,
            ),
            DisplayCommand::DrawLine {
                x1,
                y1,
                x2,
                y2,
                color,
                width,
            } => self.draw_line(
                pixmap,
                x1.saturating_add(ox),
                y1.saturating_add(oy),
                x2.saturating_add(ox),
                y2.saturating_add(oy),
                *color,
                *width,
            ),
            DisplayCommand::DrawCircle {
                x,
                y,
                radius,
                color,
                filled,
            } => self.draw_circle(
                pixmap,
                x.saturating_add(ox),
                y.saturating_add(oy),
                *radius,
                *color,
                *filled,
            ),
            DisplayCommand::DrawText {
                x,
                y,
                text,
                font_size,
                color,
            } => renderer.get_or_insert_with(TextRenderer::new).draw_text(
                pixmap,
                x.saturating_add(ox),
                y.saturating_add(oy),
                text,
                *font_size,
                *color,
            ),
            DisplayCommand::RenderAsciiArt {
                text,
                font_size,
                fg_color,
                bg_color,
            } => AsciiRenderer::render_with(
                renderer.get_or_insert_with(TextRenderer::new),
                pixmap,
                text,
                *font_size,
                *fg_color,
                *bg_color,
            ),
            DisplayCommand::DrawWindow {
                x,
                y,
                width,
                height,
                title,
                ..
            } => self.draw_window(
                pixmap,
                renderer.get_or_insert_with(TextRenderer::new),
                x.saturating_add(ox),
                y.saturating_add(oy),
                *width,
                *height,
                title,
            ),
            DisplayCommand::DrawButton {
                x,
                y,
                width,
                height,
                label,
            } => self.draw_button(
                pixmap,
                renderer.get_or_insert_with(TextRenderer::new),
                x.saturating_add(ox),
                y.saturating_add(oy),
                *width,
                *height,
                label,
            ),
            DisplayCommand::DrawTextBox {
                x,
                y,
                width,
                height,
                text,
                placeholder,
            } => self.draw_textbox(
                pixmap,
                renderer.get_or_insert_with(TextRenderer::new),
                x.saturating_add(ox),
                y.saturating_add(oy),
                *width,
                *height,
                text,
                placeholder,
            ),
        }
    }

    fn set_background(&self, pixmap: &mut Pixmap, color: Color) {
        let sk_color = color_to_tiny_skia(color);
        pixmap.fill(sk_color);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_rectangle(
        &self,
        pixmap: &mut Pixmap,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: Color,
        filled: bool,
    ) {
        let Some(rect) =
            tiny_skia::Rect::from_xywh(x as f32, y as f32, width as f32, height as f32)
        else {
            return;
        };
        let mut path_builder = PathBuilder::new();
        path_builder.push_rect(rect);
        let Some(path) = path_builder.finish() else {
            return;
        };

        let mut paint = Paint::default();
        paint.set_color(color_to_tiny_skia(color));
        paint.anti_alias = true;

        if filled {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        } else {
            let stroke = Stroke {
                width: 1.0,
                ..Default::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        &self,
        pixmap: &mut Pixmap,
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
        color: Color,
        width: u32,
    ) {
        let mut path_builder = PathBuilder::new();
        path_builder.move_to(x1 as f32, y1 as f32);
        path_builder.line_to(x2 as f32, y2 as f32);
        let Some(path) = path_builder.finish() else {
            return;
        };

        let mut paint = Paint::default();
        paint.set_color(color_to_tiny_skia(color));
        paint.anti_alias = true;

        let stroke = Stroke {
            width: width as f32,
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn draw_circle(
        &self,
        pixmap: &mut Pixmap,
        x: u32,
        y: u32,
        radius: u32,
        color: Color,
        filled: bool,
    ) {
        let mut path_builder = PathBuilder::new();
        path_builder.push_circle(x as f32, y as f32, radius as f32);
        let Some(path) = path_builder.finish() else {
            return;
        };

        let mut paint = Paint::default();
        paint.set_color(color_to_tiny_skia(color));
        paint.anti_alias = true;

        if filled {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        } else {
            let stroke = Stroke {
                width: 1.0,
                ..Default::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_window(
        &self,
        pixmap: &mut Pixmap,
        text_renderer: &mut TextRenderer,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        title: &str,
    ) {
        // Draw window background
        self.draw_rectangle(pixmap, x, y, width, height, Color::LIGHT_GRAY, true);

        // Draw window border
        self.draw_rectangle(pixmap, x, y, width, height, Color::DARK_GRAY, false);

        // Draw title bar
        self.draw_rectangle(pixmap, x, y, width, 30, Color::BLUE, true);

        // Draw title text
        text_renderer.draw_text(
            pixmap,
            x.saturating_add(10),
            y.saturating_add(20),
            title,
            14,
            Color::WHITE,
        );
    }

    fn draw_button(
        &self,
        pixmap: &mut Pixmap,
        text_renderer: &mut TextRenderer,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        label: &str,
    ) {
        // Draw button background
        self.draw_rectangle(pixmap, x, y, width, height, Color::GRAY, true);

        // Draw button border
        self.draw_rectangle(pixmap, x, y, width, height, Color::BLACK, false);

        // Draw button label (centered)
        let label_width = u32::try_from(label.chars().count())
            .unwrap_or(u32::MAX)
            .saturating_mul(7);
        let label_x = x.saturating_add((width / 2).saturating_sub(label_width / 2));
        let label_y = y.saturating_add(height / 2).saturating_add(5);
        text_renderer.draw_text(pixmap, label_x, label_y, label, 14, Color::BLACK);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_textbox(
        &self,
        pixmap: &mut Pixmap,
        text_renderer: &mut TextRenderer,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        text: &str,
        placeholder: &Option<String>,
    ) {
        // Draw textbox background
        self.draw_rectangle(pixmap, x, y, width, height, Color::WHITE, true);

        // Draw textbox border
        self.draw_rectangle(pixmap, x, y, width, height, Color::GRAY, false);

        // Draw text or placeholder
        if text.is_empty() {
            if let Some(ph) = placeholder {
                text_renderer.draw_text(
                    pixmap,
                    x.saturating_add(5),
                    y.saturating_add(height / 2).saturating_add(5),
                    ph,
                    14,
                    Color::GRAY,
                );
            }
        } else {
            text_renderer.draw_text(
                pixmap,
                x.saturating_add(5),
                y.saturating_add(height / 2).saturating_add(5),
                text,
                14,
                Color::BLACK,
            );
        }
    }
}

/// Convert a Color to tiny-skia Color
fn color_to_tiny_skia(color: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.a)
}

/// Convert tiny-skia Pixmap to image::ImageBuffer
fn pixmap_to_image_buffer(pixmap: &Pixmap) -> Result<ImageBuffer<Rgb<u8>, Vec<u8>>, CanvasError> {
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(pixmap.width() as usize * pixmap.height() as usize * 3)
        .map_err(|_| CanvasError("canvas RGB allocation failed"))?;
    for pixel in pixmap.pixels() {
        rgb.extend_from_slice(&[pixel.red(), pixel.green(), pixel.blue()]);
    }
    ImageBuffer::from_raw(pixmap.width(), pixmap.height(), rgb)
        .ok_or(CanvasError("invalid RGB surface"))
}

/// Deep command trees are also destroyed iteratively, including rejected input.
fn drop_commands(mut commands: Vec<DisplayCommand>) {
    while let Some(mut command) = commands.pop() {
        if let DisplayCommand::DrawWindow { content, .. } = &mut command {
            commands.append(content);
        }
    }
}

impl Drop for DisplayCanvas {
    fn drop(&mut self) {
        drop_commands(std::mem::take(&mut self.commands));
    }
}
