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
    renderer: TextRenderer,
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

        Self {
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            renderer,
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
        let Self { swash_cache, atlas, renderer, buffers, placements, viewport: vp } = self;

        placements.clear();
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

        let areas = placements.iter().zip(buffers.iter()).map(|(placement, buffer)| TextArea {
            buffer,
            left: placement.left,
            top: placement.top,
            scale: 1.0,
            bounds: placement.bounds,
            default_color: placement.color,
            custom_glyphs: &[],
        });

        if let Err(e) = renderer.prepare(device, queue, font_system, atlas, vp, areas, swash_cache)
        {
            eprintln!("text prepare failed: {e}");
        }
    }

    /// Draws the prepared text. Called after the rectangles, so labels sit on
    /// top of the surfaces they belong to — paint order is tree order (§6.3).
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.placements.is_empty() {
            return;
        }
        if let Err(e) = self.renderer.render(&self.atlas, &self.viewport, pass) {
            eprintln!("text render failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Approximate;

    #[test]
    fn the_font_measures_narrower_than_the_approximation() {
        // The stub errs wide on purpose. If a real font ever measured wider,
        // labels would overflow the boxes laid out for them — so this is the
        // direction of the error, asserted rather than assumed.
        let mut fonts = FontSystem::new();
        let mut real = FontMeasure::new(&mut fonts);

        for sample in ["write the compiler", "Clear done", "todo — 2/3 done", "x"] {
            let (measured, height) = real.measure(sample, 16.0);
            let (approximated, _) = Approximate.measure(sample, 16.0);
            assert!(measured > 0.0, "{sample:?} measured as nothing");
            assert!(height > 0.0, "{sample:?} has no height");
            assert!(
                measured <= approximated,
                "{sample:?}: font {measured} exceeded approximation {approximated}"
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
