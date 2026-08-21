//! Paints a render command array with wgpu.
//!
//! One instanced quad per `Command::Rect`: the vertex shader builds the corners
//! from instance data, so the CPU uploads 8 floats per rectangle and issues a
//! single draw call for the whole frame. That is the id Tech lesson from
//! §17 in miniature — do the thinking at layout time so
//! the hot loop does almost nothing.

use wgpu::util::DeviceExt;

use crate::scene::{Command, Frame};

/// Colours are authored the way a designer reads them — sRGB, as in "0.35,
/// 0.55, 0.95 is a medium blue". The surface format is sRGB, and the GPU
/// converts linear to sRGB when writing to it, so what the shader must supply
/// is the *linear* value. Skipping this makes everything washed out: 0.05
/// arrives on screen as 0.25.
fn to_linear(channel: f32) -> f32 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

/// One rectangle, as the GPU sees it.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    /// x, y, width, height, in logical pixels.
    rect: [f32; 4],
    color: [f32; 4],
}

/// Viewport size, so the shader can map pixels to clip space.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Viewport {
    size: [f32; 2],
    _padding: [f32; 2],
}

const SHADER: &str = r#"
struct Viewport { size: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> viewport: Viewport;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(
    @builtin(vertex_index) index: u32,
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
) -> VertexOut {
    // Two triangles, wound as a quad.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let corner = corners[index];
    let pixel = rect.xy + corner * rect.zw;

    // Pixels are y-down from the top left; clip space is y-up from the centre.
    let ndc = vec2<f32>(
        pixel.x / viewport.size.x * 2.0 - 1.0,
        1.0 - pixel.y / viewport.size.y * 2.0,
    );

    var out: VertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs(in: VertexOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// One draw call: the instances it covers, and the scissor they are confined
/// to. A frame with no clips is one batch, so the common case costs exactly
/// what it did before clipping existed.
#[derive(Debug, Clone)]
struct Batch {
    /// `None` means the whole viewport.
    scissor: Option<[f32; 4]>,
    range: std::ops::Range<u32>,
}

pub struct Painter {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    viewport_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    /// How many instances the current buffer can hold before it must grow.
    capacity: usize,
    instances: Vec<Instance>,
    batches: Vec<Batch>,
    /// Kept from `prepare`, because a scissor is in whole pixels and has to be
    /// clamped to something.
    viewport: (f32, f32),
}

/// The overlap of two `[x, y, w, h]` rectangles; empty where they do not meet.
fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let left = a[0].max(b[0]);
    let top = a[1].max(b[1]);
    let right = (a[0] + a[2]).min(b[0] + b[2]);
    let bottom = (a[1] + a[3]).min(b[1] + b[3]);
    [left, top, (right - left).max(0.0), (bottom - top).max(0.0)]
}

/// A scissor rectangle in whole pixels, clamped to the attachment. `None` means
/// nothing would be drawn — wgpu rejects a scissor that leaves the target, so
/// the batch is skipped rather than clamped into a lie.
fn scissor(rect: [f32; 4], viewport: (f32, f32)) -> Option<(u32, u32, u32, u32)> {
    let (width, height) = (viewport.0.max(1.0) as u32, viewport.1.max(1.0) as u32);
    let x = (rect[0].floor().max(0.0) as u32).min(width);
    let y = (rect[1].floor().max(0.0) as u32).min(height);
    let right = ((rect[0] + rect[2]).ceil().max(0.0) as u32).min(width);
    let bottom = ((rect[1] + rect[3]).ceil().max(0.0) as u32).min(height);
    (right > x && bottom > y).then_some((x, y, right - x, bottom - y))
}

/// Ends the batch that was accumulating and starts the next. A batch with no
/// instances is not worth a draw call, so it is dropped rather than emitted.
fn close_batch(batches: &mut Vec<Batch>, start: &mut u32, end: u32, scissor: Option<[f32; 4]>) {
    if end > *start {
        batches.push(Batch { scissor, range: *start..end });
    }
    *start = end;
}

/// Rectangles a fresh painter can hold before its buffer is reallocated.
const INITIAL_CAPACITY: usize = 256;

impl Painter {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("strand-ui"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let viewport_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport"),
            contents: bytemuck::bytes_of(&Viewport { size: [1.0, 1.0], _padding: [0.0; 2] }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewport-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport-bind-group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("strand-ui-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("strand-ui-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    // One step per rectangle, not per vertex.
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 1,
                        },
                    ],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-instances"),
            size: (std::mem::size_of::<Instance>() * INITIAL_CAPACITY) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group,
            viewport_buffer,
            instance_buffer,
            capacity: INITIAL_CAPACITY,
            instances: Vec::with_capacity(INITIAL_CAPACITY),
            batches: Vec::new(),
            viewport: (1.0, 1.0),
        }
    }

    /// Uploads a frame. Returns how many rectangles will be drawn.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Frame,
        viewport: (f32, f32),
    ) -> u32 {
        queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::bytes_of(&Viewport {
                size: [viewport.0.max(1.0), viewport.1.max(1.0)],
                _padding: [0.0; 2],
            }),
        );

        self.viewport = viewport;
        self.instances.clear();
        self.batches.clear();

        // Clips nest, so the active region is a stack and each entry is already
        // intersected with the one below it. A clip boundary is exactly where
        // one draw call has to end and the next begin.
        let mut clips: Vec<[f32; 4]> = Vec::new();
        let mut start: u32 = 0;

        for command in &frame.commands {
            match command {
                Command::Rect { x, y, width, height, color } => {
                    self.instances.push(Instance {
                        rect: [*x, *y, *width, *height],
                        color: [
                            to_linear(color.r),
                            to_linear(color.g),
                            to_linear(color.b),
                            // Alpha is already linear; it is not a colour.
                            color.a,
                        ],
                    });
                }
                // Text goes through glyphon, which clips itself from the same
                // command stream.
                Command::Text { .. } => {}
                Command::ClipStart { x, y, width, height } => {
                    let region = [*x, *y, *width, *height];
                    let nested = match clips.last() {
                        Some(outer) => intersect(*outer, region),
                        None => region,
                    };
                    let len = self.instances.len() as u32;
                    close_batch(&mut self.batches, &mut start, len, clips.last().copied());
                    clips.push(nested);
                }
                Command::ClipEnd => {
                    let len = self.instances.len() as u32;
                    close_batch(&mut self.batches, &mut start, len, clips.last().copied());
                    clips.pop();
                }
            }
        }
        let len = self.instances.len() as u32;
        close_batch(&mut self.batches, &mut start, len, clips.last().copied());

        if self.instances.len() > self.capacity {
            self.capacity = self.instances.len().next_power_of_two();
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ui-instances"),
                size: (std::mem::size_of::<Instance>() * self.capacity) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        if !self.instances.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&self.instances));
        }
        self.instances.len() as u32
    }

    /// Draws the frame prepared by the last `prepare` call.
    ///
    /// One draw call per clip region. An unclipped frame is a single batch, so
    /// this is the same single call it always was.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, count: u32) {
        if count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));

        let (width, height) = (self.viewport.0.max(1.0) as u32, self.viewport.1.max(1.0) as u32);
        for batch in &self.batches {
            match batch.scissor {
                Some(rect) => match scissor(rect, self.viewport) {
                    Some((x, y, w, h)) => pass.set_scissor_rect(x, y, w, h),
                    // Clipped away entirely: nothing to draw here.
                    None => continue,
                },
                None => pass.set_scissor_rect(0, 0, width, height),
            }
            // Six vertices make the quad; the instance range makes the batch.
            pass.draw(0..6, batch.range.clone());
        }
    }
}
