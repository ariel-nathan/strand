//! Platform-owned renderer — M0.
//!
//! Note the inversion the design doc implies but does not spell out: winit
//! must own the process main thread on Windows and macOS, so *this* is the
//! entry point and the actor runtime is started as its guest. App code gets
//! no handle to this thread, which is a stronger guarantee than "don't block
//! the main thread" — there is no main thread reachable from Strand code.

use std::sync::Arc;

pub mod compositor;
pub mod paint;
pub mod scene;

use compositor::{InputEvent, InputSender, SceneReceiver};
use paint::Painter;
use scene::{Color, Frame, HitId, Layouter, Node, Sizing, Style, TextStyle};

use anyhow::{anyhow, Result};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// Live GPU resources, created once the window exists.
struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    painter: Painter,
}

impl Gpu {
    async fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(window)?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("no suitable GPU adapter: {e}"))?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("strand-device"),
                ..Default::default()
            })
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow!("surface is not supported by this adapter"))?;
        if let Some(srgb) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = srgb;
        }
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        config.present_mode = wgpu::PresentMode::Fifo;

        surface.configure(&device, &config);

        let painter = Painter::new(&device, config.format);
        Ok(Self { surface, device, queue, config, painter })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// Paints one frame's command array (§6.1).
    fn render(&mut self, frame: &Frame) -> Result<()> {
        let viewport = (self.config.width as f32, self.config.height as f32);
        let count = self.painter.prepare(&self.device, &self.queue, frame, viewport);

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            // Surface changed under us: reconfigure and let the next frame land.
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(())
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow!("surface validation error"))
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.06,
                            b: 0.09,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                ..Default::default()
            });
            self.painter.draw(&mut pass, count);
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }
}

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    layouter: Layouter,
    /// The tree to paint. Replaced wholesale each time the app submits one.
    scene: Option<Node>,
    /// Submissions from app actors (§6.1). Polled, never waited on.
    scenes: Option<SceneReceiver>,
    /// Frame counter, so the compositor's own rate is measurable rather than
    /// merely claimed.
    frames: u32,
    /// Scenes drawn, to show how far the app fell behind.
    updates: u32,
    last_report: Option<std::time::Instant>,
    /// Events routed back to the app (§6.1). The app never hit-tests.
    input: Option<InputSender>,
    cursor: (f32, f32),
    hovered: Option<HitId>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("Strand — M0");
        match event_loop.create_window(attrs) {
            Ok(window) => {
                let window = Arc::new(window);
                match pollster::block_on(Gpu::new(window.clone())) {
                    Ok(gpu) => {
                        self.gpu = Some(gpu);
                        self.window = Some(window);
                    }
                    Err(e) => {
                        eprintln!("gpu init failed: {e:#}");
                        event_loop.exit();
                    }
                }
            }
            Err(e) => {
                eprintln!("window creation failed: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                // Hit-test against the frame actually on screen.
                let hit = self.layouter.frame().hit_test(self.cursor.0, self.cursor.1);
                if hit != self.hovered {
                    if let Some(input) = &self.input {
                        if let Some(left) = self.hovered {
                            input.send(InputEvent::PointerLeave { id: left });
                        }
                        if let Some(entered) = hit {
                            input.send(InputEvent::PointerEnter { id: entered });
                        }
                    }
                    self.hovered = hit;
                }
            }

            WindowEvent::MouseInput { state, .. } => {
                let (Some(input), Some(id)) = (&self.input, self.hovered) else { return };
                let (x, y) = self.cursor;
                input.send(match state {
                    winit::event::ElementState::Pressed => InputEvent::PointerDown { id, x, y },
                    winit::event::ElementState::Released => InputEvent::PointerUp { id, x, y },
                });
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(gpu) = &mut self.gpu {
                    let viewport = (gpu.config.width as f32, gpu.config.height as f32);
                    // Take whatever the app has most recently submitted. This
                    // never blocks, so a slow actor cannot hold up the frame.
                    if let Some(scenes) = &mut self.scenes {
                        if scenes.poll() {
                            self.scene = scenes.current().cloned();
                            self.updates += 1;
                        }
                    }

                    self.frames += 1;
                    let now = std::time::Instant::now();
                    let since = *self.last_report.get_or_insert(now);
                    if now.duration_since(since).as_secs_f32() >= 1.0 {
                        // The §6.1 claim, measured: compositor frames should
                        // stay high even when app updates collapse.
                        eprintln!(
                            "compositor {:>4} fps | app submitted {:>3} frames",
                            self.frames, self.updates
                        );
                        self.frames = 0;
                        self.updates = 0;
                        self.last_report = Some(now);
                    }
                    let tree = self.scene.get_or_insert_with(demo_scene);
                    let frame = self.layouter.layout(tree, viewport);
                    if let Err(e) = gpu.render(frame) {
                        eprintln!("frame dropped: {e}");
                    }
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

/// A stand-in UI until an app actor submits one: a header, a sidebar and a
/// few rows, enough to show layout and painting working together.
fn demo_scene() -> Node {
    let panel = Color::rgb(0.13, 0.14, 0.18);
    let accent = Color::rgb(0.35, 0.55, 0.95);
    let muted = Color::rgb(0.22, 0.23, 0.28);

    let row = |shade: Color| Node::Box {
        style: Style {
            width: Sizing::Grow,
            height: Sizing::Fixed(28.0),
            background: Some(shade),
            ..Default::default()
        },
    };

    Node::column(
        Style { width: Sizing::Grow, height: Sizing::Grow, padding: 16.0, gap: 12.0, ..Default::default() },
        vec![
            // header
            Node::Box {
                style: Style {
                    width: Sizing::Grow,
                    height: Sizing::Fixed(48.0),
                    background: Some(accent),
                    ..Default::default()
                },
            },
            Node::row(
                Style { width: Sizing::Grow, height: Sizing::Grow, gap: 12.0, ..Default::default() },
                vec![
                    // sidebar
                    Node::Box {
                        style: Style {
                            width: Sizing::Percent(0.28),
                            height: Sizing::Grow,
                            background: Some(panel),
                            ..Default::default()
                        },
                    },
                    // list
                    Node::column(
                        Style {
                            width: Sizing::Grow,
                            height: Sizing::Grow,
                            padding: 12.0,
                            gap: 8.0,
                            background: Some(panel),
                            ..Default::default()
                        },
                        vec![row(muted), row(muted), row(accent), row(muted)],
                    ),
                ],
            ),
            Node::text("strand", TextStyle::default()),
        ],
    )
}

/// Takes over the calling thread (which must be the main thread) with the
/// window + compositor loop.
pub fn run() -> Result<()> {
    run_with(None, None)
}

/// Runs the compositor, drawing scenes submitted by app actors.
///
/// This takes over the calling thread, which must be the main thread — winit
/// requires it on Windows and macOS. The actor runtime therefore runs as a
/// guest of the compositor rather than the other way round, which is a
/// stronger guarantee than §6.1 states: app code has no handle to this thread
/// at all.
pub fn run_with(scenes: Option<SceneReceiver>, input: Option<InputSender>) -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App { scenes, input, ..Default::default() };
    event_loop.run_app(&mut app)?;
    Ok(())
}
