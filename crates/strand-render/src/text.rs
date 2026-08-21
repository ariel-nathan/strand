//! Text rendering via glyphon (§6.4).
//!
//! §12 calls text a tarpit and prescribes ruthless scope: one font, Latin, no
//! shaping ambitions. glyphon does the hard parts — atlas, cache, swash
//! rasterisation — so this is the thin layer that turns `Command::Text` into
//! something on screen.
//!
//! Known mismatch, documented rather than hidden: layout still *measures* text
//! with the monospace approximation in `scene`, while glyphon *renders* it with
//! a real font. Where the two disagree, a label can overflow the box laid out
//! for it. Closing that means measuring through the same `FontSystem` the
//! renderer uses, which is the next step for text and not this one.

use glyphon::{
    Attrs, Buffer, Cache, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport,
};

use crate::scene::{Command, Frame};

pub struct TextPainter {
    font_system: FontSystem,
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
            font_system: FontSystem::new(),
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
        frame: &Frame,
        viewport: (f32, f32),
    ) {
        self.viewport.update(
            queue,
            Resolution { width: viewport.0.max(1.0) as u32, height: viewport.1.max(1.0) as u32 },
        );

        // Split the borrow so the buffers can be read while the renderer and
        // atlas are written.
        let Self { font_system, swash_cache, atlas, renderer, buffers, placements, viewport: vp } =
            self;

        placements.clear();
        let mut index = 0;
        for command in &frame.commands {
            let Command::Text { x, y, size, color, text } = command else { continue };

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
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: viewport.0 as i32,
                bottom: viewport.1 as i32,
            },
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
