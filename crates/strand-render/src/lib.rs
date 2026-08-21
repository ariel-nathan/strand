//! Platform-owned renderer — M0.
//!
//! Note the inversion the design doc implies but does not spell out: winit
//! must own the process main thread on Windows and macOS, so *this* is the
//! entry point and the actor runtime is started as its guest. App code gets
//! no handle to this thread, which is a stronger guarantee than "don't block
//! the main thread" — there is no main thread reachable from Strand code.

use std::sync::Arc;

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

        Ok(Self { surface, device, queue, config })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// M0 paints a flat colour. M3 replaces this with scene-graph traversal.
    fn render(&mut self) -> Result<()> {
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
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
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
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(gpu) = &mut self.gpu {
                    if let Err(e) = gpu.render() {
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

/// Takes over the calling thread (which must be the main thread) with the
/// window + compositor loop.
pub fn run() -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::default();
    event_loop.run_app(&mut app)?;
    Ok(())
}
