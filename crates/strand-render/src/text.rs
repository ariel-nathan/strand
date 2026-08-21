//! Text rendering via glyphon (§6.4).
//!
//! §12 calls text a tarpit and prescribes ruthless scope: one font, Latin, no
//! shaping ambitions. glyphon does the hard parts — atlas, cache, swash
//! rasterisation — so this is the thin layer that turns `Command::Text` into
//! something on screen.
//!
//! Layout and rendering share one `FontSystem`, so a label is measured by the
//! font that draws it. The alternative — approximating during layout — means
//! every box is sized from a number the renderer disagrees with, and the error
//! compounds through nested layouts.

use glyphon::{
    Attrs, Buffer, Cache, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport,
};

use crate::paint::Layer;
use crate::scene::{Command, Frame, Measure};

/// Measures with the same font stack that renders, so layout and painting
/// agree. Holds one scratch buffer rather than allocating per string.
pub struct FontMeasure<'a> {
    fonts: &'a mut FontSystem,
    scratch: Buffer,
}

impl<'a> FontMeasure<'a> {
    pub fn new(fonts: &'a mut FontSystem) -> Self {
        let scratch = Buffer::new(fonts, Metrics::new(16.0, 20.0));
        Self { fonts, scratch }
    }
}

impl Measure for FontMeasure<'_> {
    fn measure(&mut self, text: &str, size: f32) -> (f32, f32) {
        let line_height = size * 1.25;
        self.scratch.set_metrics(Metrics::new(size, line_height));
        self.scratch.set_size(None, None);
        self.scratch.set_text(
            text,
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.scratch.shape_until_scroll(self.fonts, false);

        let mut width: f32 = 0.0;
        let mut lines: f32 = 0.0;
        for run in self.scratch.layout_runs() {
            width = width.max(run.line_w);
            lines += 1.0;
        }
        (width.ceil(), (lines.max(1.0) * line_height).ceil())
    }
}

pub struct TextPainter {
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    /// One renderer per layer. glyphon draws everything a renderer prepared in
    /// a single call, so two layers need two of them; they share the atlas, so
    /// a glyph cached for one is cached for both.
    renderer: TextRenderer,
    overlay_renderer: TextRenderer,
    /// One shaped buffer per text command in the current frame. Kept between
    /// frames so the allocation is reused, in the same spirit as the layouter.
    buffers: Vec<Buffer>,
    /// Where each buffer goes, and in what colour.
    placements: Vec<Placement>,
}

struct Placement {
    left: f32,
    top: f32,
    color: glyphon::Color,
    /// The clip region this run fell inside (§6.1). glyphon takes bounds per
    /// text area, so scrolled text is trimmed by the same command stream that
    /// trims the rectangles — no second clipping mechanism.
    bounds: TextBounds,
}

/// How many text runs in `frame` belong to the application rather than to the
/// debug overlay laid over it.
///
/// Pulled out of `prepare` because it is the whole of the layering decision and
/// the rest of `prepare` needs a GPU to run.
fn app_runs(frame: &Frame) -> usize {
    frame
        .app_commands()
        .iter()
        .filter(|command| matches!(command, Command::Text { .. }))
        .count()
}

/// Where one shaped run goes. A free function rather than a closure so that
/// the borrow of the buffer outlives the iterator built from it.
fn area<'a>((placement, buffer): (&Placement, &'a Buffer)) -> TextArea<'a> {
    TextArea {
        buffer,
        left: placement.left,
        top: placement.top,
        scale: 1.0,
        bounds: placement.bounds,
        default_color: placement.color,
        custom_glyphs: &[],
    }
}

/// sRGB bytes, which is what glyphon expects.
fn to_srgb8(channel: f32) -> u8 {
    (channel.clamp(0.0, 1.0) * 255.0).round() as u8
}

impl TextPainter {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        let overlay_renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);

        Self {
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            renderer,
            overlay_renderer,
            buffers: Vec::new(),
            placements: Vec::new(),
        }
    }

    /// Shapes and uploads every text command in the frame.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        font_system: &mut FontSystem,
        frame: &Frame,
        viewport: (f32, f32),
    ) {
        self.viewport.update(
            queue,
            Resolution { width: viewport.0.max(1.0) as u32, height: viewport.1.max(1.0) as u32 },
        );

        // Split the borrow so the buffers can be read while the renderer and
        // atlas are written.
        let Self {
            swash_cache,
            atlas,
            renderer,
            overlay_renderer,
            buffers,
            placements,
            viewport: vp,
        } = self;

        placements.clear();
        // How many of the runs about to be shaped belong to the application.
        // Counted rather than split into two vectors, because the buffers are
        // indexed by position and reused between frames.
        let split = app_runs(frame);
        let mut index = 0;
        let whole = TextBounds {
            left: 0,
            top: 0,
            right: viewport.0 as i32,
            bottom: viewport.1 as i32,
        };
        // Clips nest, and each entry is already intersected with the one below.
        let mut clips: Vec<TextBounds> = Vec::new();

        for command in &frame.commands {
            let Command::Text { x, y, size, color, text } = command else {
                match command {
                    Command::ClipStart { x, y, width, height } => {
                        let region = TextBounds {
                            left: *x as i32,
                            top: *y as i32,
                            right: (x + width).ceil() as i32,
                            bottom: (y + height).ceil() as i32,
                        };
                        let nested = match clips.last() {
                            Some(outer) => TextBounds {
                                left: region.left.max(outer.left),
                                top: region.top.max(outer.top),
                                right: region.right.min(outer.right),
                                bottom: region.bottom.min(outer.bottom),
                            },
                            None => region,
                        };
                        clips.push(nested);
                    }
                    Command::ClipEnd => {
                        clips.pop();
                    }
                    _ => {}
                }
                continue;
            };

            if index == buffers.len() {
                buffers.push(Buffer::new(font_system, Metrics::new(*size, size * 1.25)));
            }
            let buffer = &mut buffers[index];
            buffer.set_metrics(Metrics::new(*size, size * 1.25));
            // No wrapping bound: layout already decided how much room there is,
            // and a surprise line break would disagree with it.
            buffer.set_size(None, None);
            buffer.set_text(
                text,
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(font_system, false);

            placements.push(Placement {
                left: *x,
                top: *y,
                color: glyphon::Color::rgba(
                    to_srgb8(color.r),
                    to_srgb8(color.g),
                    to_srgb8(color.b),
                    to_srgb8(color.a),
                ),
                bounds: clips.last().copied().unwrap_or(whole),
            });
            index += 1;
        }

        if placements.is_empty() {
            return;
        }

        // Prepared separately so each layer can be drawn after its own
        // rectangles rather than after all of them.
        let app = placements[..split].iter().zip(buffers.iter()).map(area);
        if let Err(e) = renderer.prepare(device, queue, font_system, atlas, vp, app, swash_cache) {
            eprintln!("text prepare failed: {e}");
        }
        let overlay = placements[split..].iter().zip(buffers[split..].iter()).map(area);
        if let Err(e) =
            overlay_renderer.prepare(device, queue, font_system, atlas, vp, overlay, swash_cache)
        {
            eprintln!("overlay text prepare failed: {e}");
        }
    }

    /// Draws one layer's prepared text. Called after that layer's rectangles,
    /// so a label sits on top of the surfaces it belongs to — paint order is
    /// tree order (§7.3) — and an overlay's labels sit on top of everything.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, layer: Layer) {
        if self.placements.is_empty() {
            return;
        }
        let renderer = match layer {
            Layer::App => &self.renderer,
            Layer::Overlay => &self.overlay_renderer,
        };
        if let Err(e) = renderer.render(&self.atlas, &self.viewport, pass) {
            eprintln!("text render failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Approximate;

    use crate::scene::Color;

    fn label(text: &str) -> Command {
        Command::Text {
            x: 0.0,
            y: 0.0,
            size: 12.0,
            color: Color::rgb(1.0, 1.0, 1.0),
            text: text.to_string(),
        }
    }

    fn rect() -> Command {
        Command::Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            color: Color::rgb(1.0, 1.0, 1.0),
        }
    }

    #[test]
    fn text_splits_where_the_overlay_begins() {
        // The bug this rules out: with one prepared set, every label in the
        // app draws after every rectangle — including the overlay's panel — so
        // a button caption lands on top of the instrument reporting on it.
        let frame = Frame {
            commands: vec![rect(), label("Add"), rect(), label("crash stats"), label("ui")],
            overlay_from: Some(4),
            ..Default::default()
        };
        assert_eq!(app_runs(&frame), 2, "two app labels precede the overlay");
    }

    #[test]
    fn every_run_is_the_app_when_the_overlay_is_off() {
        // The usual case: F12 has not been pressed, nothing marked a boundary,
        // and the overlay's prepared set is empty.
        let frame = Frame {
            commands: vec![label("Add"), rect(), label("Clear done")],
            ..Default::default()
        };
        assert_eq!(app_runs(&frame), 2);
        assert!(frame.overlay_commands().is_empty());
    }

    #[test]
    fn a_rectangle_between_labels_does_not_shift_the_split() {
        // `app_runs` counts text runs, and the boundary is an index into all
        // commands. Counting the wrong thing would put the last app label into
        // the overlay's set, where it would be drawn twice or not at all.
        let frame = Frame {
            commands: vec![rect(), rect(), label("Add"), rect(), label("ui")],
            overlay_from: Some(4),
            ..Default::default()
        };
        assert_eq!(app_runs(&frame), 1);
    }

    #[test]
    fn the_approximation_is_the_same_order_as_a_real_font() {
        // This test used to assert that the approximation is never narrower
        // than the font — that it "errs wide", so a label laid out without a
        // font stack is given room rather than clipped. CI showed that is not
        // a property of the code. It was a property of this machine's fonts.
        //
        // `FontSystem::new()` takes whatever the host provides. Measured at
        // 16px, font ÷ approximation came out:
        //
        //     Segoe UI (Windows)     0.81  0.88  0.93  0.91
        //     DejaVu Sans (Linux)          1.01        1.14
        //
        // So it errs wide on one host and narrow on the other, and worst of
        // all on one glyph — an average advance per character cannot bound a
        // single character, where one wide letter is the whole string.
        //
        // What is left is a sanity check, and it is worth keeping as one: it
        // catches a measurer that returns nothing, or that has the units
        // wrong, which is the failure that would actually happen. The exact
        // property wants §14's one bundled font, which would make layout
        // reproducible across machines rather than merely close.
        const SPREAD: f32 = 1.3;

        let mut fonts = FontSystem::new();
        let mut real = FontMeasure::new(&mut fonts);

        for sample in ["write the compiler", "Clear done", "todo — 2/3 done", "x"] {
            let (measured, height) = real.measure(sample, 16.0);
            let (approximated, _) = Approximate.measure(sample, 16.0);
            assert!(measured > 0.0, "{sample:?} measured as nothing");
            assert!(height > 0.0, "{sample:?} has no height");
            assert!(
                measured <= approximated * SPREAD && measured >= approximated / SPREAD,
                "{sample:?}: font {measured} and approximation {approximated} are \
                 not the same size of number"
            );
        }
    }

    #[test]
    fn measuring_is_proportional_to_the_font_size() {
        let mut fonts = FontSystem::new();
        let mut real = FontMeasure::new(&mut fonts);
        let (small, _) = real.measure("hello", 12.0);
        let (large, _) = real.measure("hello", 24.0);
        assert!(large > small * 1.5, "24pt should be far wider than 12pt: {small} vs {large}");
    }
}
