//! Renders offscreen and inspects the actual pixels.
//!
//! Written after a screenshot found four bugs that 130 passing tests missed.
//! Every one of them was correct as *data* and wrong on *screen*, which is a
//! class of bug no test over command arrays can reach: sRGB conversion, a root
//! that did not fill the viewport, and growth applied to the wrong axis.
//!
//! Specific pixels are asserted rather than whole images compared. A golden PNG
//! would break on any driver that dithers or rounds differently; a handful of
//! named coordinates says what is actually meant.
//!
//! These need a GPU adapter. Where none exists the tests skip rather than fail,
//! since a machine without one has not told us anything about the renderer.

use strand_render::inspect::{ActorStat, Inspector};
use strand_render::paint::{Layer, Painter};
use strand_render::scene::{Color, Frame, HitId, Layouter, Node, Sizing, Style};

const SIZE: u32 = 256;

struct Headless {
    device: wgpu::Device,
    queue: wgpu::Queue,
    texture: wgpu::Texture,
    painter: Painter,
}

/// The format a real window uses, so the sRGB path under test is the same one.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

impl Headless {
    fn new() -> Option<Self> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("headless"),
                ..Default::default()
            }))
            .ok()?;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let painter = Painter::new(&device, FORMAT);
        Some(Self { device, queue, texture, painter })
    }

    /// Draws a frame and reads the result back as RGBA bytes.
    fn render(&mut self, frame: &Frame) -> Vec<u8> {
        let count =
            self.painter.prepare(&self.device, &self.queue, frame, (SIZE as f32, SIZE as f32));

        let view = self.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("headless-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        // Opaque black, so anything non-black came from a command.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            // Both layers, in the order the compositor uses them.
            self.painter.draw(&mut pass, count, Layer::App);
            self.painter.draw(&mut pass, count, Layer::Overlay);
        }

        // 256 px * 4 bytes is already a multiple of the 256-byte row alignment.
        let bytes = (SIZE * SIZE * 4) as u64;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SIZE * 4),
                    rows_per_image: Some(SIZE),
                },
            },
            wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::PollType::wait_indefinitely()).expect("readback stalled");
        let data = slice.get_mapped_range().expect("mapping failed").to_vec();
        readback.unmap();
        data
    }
}

fn pixel(data: &[u8], x: u32, y: u32) -> (u8, u8, u8) {
    let at = ((y * SIZE + x) * 4) as usize;
    (data[at], data[at + 1], data[at + 2])
}

/// Allows a channel or two of rounding from the GPU.
fn close(actual: (u8, u8, u8), expected: (u8, u8, u8)) -> bool {
    let diff = |a: u8, b: u8| (a as i16 - b as i16).abs() <= 2;
    diff(actual.0, expected.0) && diff(actual.1, expected.1) && diff(actual.2, expected.2)
}

macro_rules! gpu_or_skip {
    () => {
        match Headless::new() {
            Some(headless) => headless,
            None => {
                eprintln!("no GPU adapter available; skipping pixel test");
                return;
            }
        }
    };
}

#[test]
fn a_colour_survives_the_srgb_round_trip() {
    // The bug this exists for: colours are authored in sRGB, the surface is
    // sRGB, so what lands in the texture should be what was asked for. Before
    // the fix, 0.07 (18) arrived as roughly 0.25 (63).
    let mut headless = gpu_or_skip!();

    let tree = Node::Box {
        style: Style {
            width: Sizing::Grow,
            height: Sizing::Grow,
            background: Some(Color::rgb(0.07, 0.07, 0.09)),
            ..Default::default()
        },
    };
    let mut layouter = Layouter::new();
    let data = headless.render(layouter.layout(&tree, (SIZE as f32, SIZE as f32)));

    let sampled = pixel(&data, 128, 128);
    let expected = (18, 18, 23);
    assert!(
        close(sampled, expected),
        "expected the authored colour back, got {sampled:?} (washed out means the \
         linear conversion is missing)"
    );
}

#[test]
fn a_growing_root_paints_every_corner() {
    // The bug this exists for: the root sized to its content, leaving most of
    // the window showing the clear colour.
    let mut headless = gpu_or_skip!();

    let tree = Node::column(
        Style {
            width: Sizing::Grow,
            height: Sizing::Grow,
            background: Some(Color::rgb(0.5, 0.25, 0.75)),
            ..Default::default()
        },
        vec![],
    );
    let mut layouter = Layouter::new();
    let data = headless.render(layouter.layout(&tree, (SIZE as f32, SIZE as f32)));

    for (x, y) in [(1, 1), (SIZE - 2, 1), (1, SIZE - 2), (SIZE - 2, SIZE - 2), (128, 128)] {
        let sampled = pixel(&data, x, y);
        assert!(
            sampled != (0, 0, 0),
            "({x}, {y}) is still the clear colour — the root did not fill the viewport"
        );
    }
}

#[test]
fn a_box_lands_where_layout_put_it() {
    let mut headless = gpu_or_skip!();

    // A 40x40 red box, inset 20px by the parent's padding.
    let tree = Node::column(
        Style { padding: 20.0, ..Default::default() },
        vec![Node::Box {
            style: Style {
                width: Sizing::Fixed(40.0),
                height: Sizing::Fixed(40.0),
                background: Some(Color::rgb(1.0, 0.0, 0.0)),
                ..Default::default()
            },
        }],
    );
    let mut layouter = Layouter::new();
    let data = headless.render(layouter.layout(&tree, (SIZE as f32, SIZE as f32)));

    assert!(close(pixel(&data, 40, 40), (255, 0, 0)), "the box should cover its middle");
    assert_eq!(pixel(&data, 10, 10), (0, 0, 0), "the padding should be empty");
    assert_eq!(pixel(&data, 70, 70), (0, 0, 0), "and so should past its far edge");
}

#[test]
fn growing_across_the_parent_axis_does_not_grow_the_other_one() {
    // The bug this exists for: `width: Grow` inside a column set flex_grow,
    // which made the node taller instead of wider.
    let mut headless = gpu_or_skip!();

    let tree = Node::column(
        Style { width: Sizing::Grow, height: Sizing::Grow, ..Default::default() },
        vec![Node::Box {
            style: Style {
                width: Sizing::Grow,
                height: Sizing::Fixed(30.0),
                background: Some(Color::rgb(0.0, 1.0, 0.0)),
                ..Default::default()
            },
        }],
    );
    let mut layouter = Layouter::new();
    let data = headless.render(layouter.layout(&tree, (SIZE as f32, SIZE as f32)));

    assert!(close(pixel(&data, SIZE - 2, 15), (0, 255, 0)), "it should span the full width");
    assert_eq!(pixel(&data, 128, 60), (0, 0, 0), "but stop at its fixed height");
}

#[test]
fn later_siblings_paint_over_earlier_ones() {
    // Paint order is tree order (§6.3, no z-index), and the pixels agree.
    let mut headless = gpu_or_skip!();

    let overlay = |color: Color| Node::Box {
        style: Style {
            width: Sizing::Fixed(100.0),
            height: Sizing::Fixed(100.0),
            background: Some(color),
            ..Default::default()
        },
    };
    // Two boxes in a column with a negative-free layout would separate them, so
    // nest instead: the child paints after, and over, its parent.
    let tree = Node::column(
        Style {
            width: Sizing::Fixed(100.0),
            height: Sizing::Fixed(100.0),
            background: Some(Color::rgb(1.0, 0.0, 0.0)),
            ..Default::default()
        },
        vec![overlay(Color::rgb(0.0, 0.0, 1.0))],
    );
    let mut layouter = Layouter::new();
    let data = headless.render(layouter.layout(&tree, (SIZE as f32, SIZE as f32)));

    assert!(close(pixel(&data, 50, 50), (0, 0, 255)), "the child covers the parent");
}

#[test]
fn the_debug_overlay_dims_the_app_without_hiding_it() {
    // §8.4's panel is injected render commands, not a second rendering path,
    // so it goes through the same blending as everything else. Translucent is
    // the point: an opaque panel would hide the app it is reporting on.
    let mut headless = gpu_or_skip!();

    let tree = Node::Box {
        style: Style {
            width: Sizing::Grow,
            height: Sizing::Grow,
            background: Some(Color::rgb(0.0, 1.0, 0.0)),
            ..Default::default()
        },
    };
    let mut layouter = Layouter::new();
    let viewport = (SIZE as f32, SIZE as f32);
    layouter.layout(&tree, viewport);

    let stats = [ActorStat {
        name: "counter".into(),
        arena_bytes: 65_536,
        mailbox: 0,
        fibers: 1,
        handled: 7,
        generation: 0,
        alive: true,
    }];
    Inspector { enabled: true, highlight: None }.overlay(
        layouter.frame_mut(),
        viewport,
        &stats,
    );
    let data = headless.render(layouter.frame());

    let under_panel = pixel(&data, 128, 30);
    let below_panel = pixel(&data, 128, 200);
    assert!(under_panel.1 < below_panel.1, "the panel should darken what is behind it");
    assert!(under_panel.1 > 0, "but not black it out — this is an overlay, not a curtain");
}

/// A `height`-tall scroll holding one very tall child, so anything painted
/// below `height` got there by escaping the clip.
fn overflowing_scroll(height: f32, child: Color) -> Node {
    Node::Scroll {
        style: Style {
            id: Some(HitId(1)),
            width: Sizing::Fixed(SIZE as f32),
            height: Sizing::Fixed(height),
            ..Default::default()
        },
        offset: 0.0,
        bar: None,
        children: vec![Node::Box {
            style: Style {
                width: Sizing::Fixed(SIZE as f32),
                height: Sizing::Fixed(300.0),
                background: Some(child),
                ..Default::default()
            },
        }],
    }
}

#[test]
fn a_scroll_clips_its_content_on_the_gpu() {
    // Clip commands are a claim until the scissor honours them. Without
    // set_scissor_rect the 300px child paints straight over everything below,
    // and every command-array test still passes.
    let mut headless = gpu_or_skip!();

    let mut layouter = Layouter::new();
    let tree = overflowing_scroll(100.0, Color::rgb(1.0, 0.0, 0.0));
    let data = headless.render(layouter.layout(&tree, (SIZE as f32, SIZE as f32)));

    assert!(close(pixel(&data, 128, 50), (255, 0, 0)), "inside the scroll it paints");
    assert_eq!(pixel(&data, 128, 150), (0, 0, 0), "and past the clip it stops");
}

#[test]
fn drawing_resumes_once_the_clip_closes() {
    // A scissor left set would trim everything drawn afterwards — the sibling
    // below, and the debug overlay with it.
    let mut headless = gpu_or_skip!();

    let tree = Node::column(
        Style::default(),
        vec![
            overflowing_scroll(60.0, Color::rgb(1.0, 0.0, 0.0)),
            Node::Box {
                style: Style {
                    width: Sizing::Fixed(SIZE as f32),
                    height: Sizing::Fixed(60.0),
                    background: Some(Color::rgb(0.0, 1.0, 0.0)),
                    ..Default::default()
                },
            },
        ],
    );
    let mut layouter = Layouter::new();
    let data = headless.render(layouter.layout(&tree, (SIZE as f32, SIZE as f32)));

    assert!(close(pixel(&data, 128, 30), (255, 0, 0)), "the scroll shows its content");
    assert!(close(pixel(&data, 128, 90), (0, 255, 0)), "and the sibling below is not trimmed");
    assert_eq!(pixel(&data, 128, 150), (0, 0, 0), "while the overflow stays clipped");
}
