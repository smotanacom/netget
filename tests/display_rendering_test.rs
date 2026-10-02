//! CPU-only raster regressions; no model requests or graphics devices are used.

use netget::display::{Color, DisplayCanvas, DisplayCommand, TextRenderer};

#[test]
fn empty_canvases_and_degenerate_shapes_do_not_panic() {
    for (width, height) in [(0, 0), (0, 8), (8, 0)] {
        assert_eq!(
            DisplayCanvas::new(width, height).render().dimensions(),
            (width, height)
        );
    }
    let mut canvas = DisplayCanvas::new(8, 8);
    canvas.add_command(DisplayCommand::SetBackground {
        color: Color::WHITE,
    });
    for (width, height) in [(0, 0), (0, 4), (4, 0)] {
        canvas.add_command(DisplayCommand::DrawRectangle {
            x: 0,
            y: 0,
            width,
            height,
            color: Color::RED,
            filled: true,
        });
    }
    canvas.add_command(DisplayCommand::DrawCircle {
        x: 0,
        y: 0,
        radius: 0,
        color: Color::RED,
        filled: true,
    });
    assert!(canvas
        .render()
        .pixels()
        .all(|pixel| pixel.0 == [255, 255, 255]));
}

#[test]
fn offscreen_nested_window_coordinates_do_not_wrap_or_panic() {
    let mut canvas = DisplayCanvas::new(8, 8);
    canvas.add_command(DisplayCommand::SetBackground {
        color: Color::WHITE,
    });
    canvas.add_command(DisplayCommand::DrawWindow {
        x: u32::MAX,
        y: u32::MAX,
        width: 10,
        height: 10,
        title: String::new(),
        content: vec![DisplayCommand::DrawRectangle {
            x: 2,
            y: 2,
            width: 4,
            height: 4,
            color: Color::RED,
            filled: true,
        }],
    });
    assert!(canvas
        .render()
        .pixels()
        .all(|pixel| pixel.0 == [255, 255, 255]));
}

#[test]
fn text_alpha_and_line_baselines_are_preserved() {
    let mut renderer = TextRenderer::new();
    let mut invisible = tiny_skia::Pixmap::new(160, 100).unwrap();
    renderer.draw_text(&mut invisible, 4, 4, "Hello", 24, Color::rgba(255, 0, 0, 0));
    assert!(invisible.pixels().iter().all(|p| p.alpha() == 0));

    let mut visible = tiny_skia::Pixmap::new(160, 100).unwrap();
    renderer.draw_text(
        &mut visible,
        4,
        4,
        "Hello\nHello",
        24,
        Color::rgba(255, 0, 0, 128),
    );
    assert!(visible.pixels().iter().all(|p| p.alpha() <= 128));
    let painted_rows: Vec<usize> = visible
        .pixels()
        .chunks(160)
        .enumerate()
        .filter_map(|(y, row)| row.iter().any(|p| p.alpha() > 0).then_some(y))
        .collect();
    assert!(
        !painted_rows.is_empty(),
        "system fonts must render the fixture"
    );
    assert!(
        painted_rows.last().unwrap() - painted_rows[0] > 24,
        "two lines need distinct baselines: {painted_rows:?}"
    );
}

#[test]
fn zero_font_size_and_extreme_origins_are_noops() {
    let mut renderer = TextRenderer::new();
    let mut pixmap = tiny_skia::Pixmap::new(8, 8).unwrap();
    renderer.draw_text(&mut pixmap, 0, 0, "hello", 0, Color::WHITE);
    renderer.draw_text(&mut pixmap, 0, 0, "hello", u32::MAX, Color::WHITE);
    renderer.draw_text(&mut pixmap, u32::MAX, u32::MAX, "hello", 24, Color::WHITE);
    assert!(pixmap.pixels().iter().all(|p| p.alpha() == 0));
}

#[test]
fn oversized_surfaces_and_command_trees_are_fallible_without_allocating_pixels() {
    assert!(DisplayCanvas::new(u32::MAX, u32::MAX).try_render().is_err());
    let mut canvas = DisplayCanvas::new(1, 1);
    let mut command = DisplayCommand::Clear;
    for _ in 0..20_000 {
        command = DisplayCommand::DrawWindow {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            title: String::new(),
            content: vec![command],
        };
    }
    canvas.add_command(command);
    assert!(canvas.try_render().is_err());
    // Dropping or clearing rejected deep trees must not recurse either.
    canvas.clear_commands();
    assert!(canvas.try_render().is_ok());
}

#[test]
fn nested_windows_preserve_draw_order_and_accumulated_coordinates() {
    let mut canvas = DisplayCanvas::new(24, 96);
    canvas.add_command(DisplayCommand::DrawWindow {
        x: 2,
        y: 1,
        width: 20,
        height: 90,
        title: String::new(),
        content: vec![DisplayCommand::DrawWindow {
            x: 3,
            y: 2,
            width: 12,
            height: 50,
            title: String::new(),
            content: vec![DisplayCommand::DrawRectangle {
                x: 1,
                y: 1,
                width: 3,
                height: 3,
                color: Color::RED,
                filled: true,
            }],
        }],
    });
    let image = canvas.try_render().unwrap();
    // 2+3+1, 1+30+2+30+1: child content follows both title bars.
    assert_eq!(image.get_pixel(7, 65).0, [255, 0, 0]);
}
