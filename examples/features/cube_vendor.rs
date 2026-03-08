// Minimal shim to run upstream cube (examples/features/src/cube) either with winit
// or using our headless KMS library (when built with --features kms_runner).
// We vendor the upstream files under examples/features/upstream/cube/ and include
// them here so wgpu::include_wgsl!("shader.wgsl") resolves correctly.

#![allow(dead_code)]
use anyhow::Result;

// Provide a tiny framework module that the upstream cube expects
mod framework {
    use super::*;
    pub trait Example {
        fn optional_features() -> wgpu::Features { wgpu::Features::empty() }
        fn init(config: &wgpu::SurfaceConfiguration, adapter: &wgpu::Adapter, device: &wgpu::Device, queue: &wgpu::Queue) -> Self where Self: Sized;
        fn update(&mut self, _event: winit::event::WindowEvent) {}
        fn resize(&mut self, _config: &wgpu::SurfaceConfiguration, _device: &wgpu::Device, _queue: &wgpu::Queue) {}
        fn render(&mut self, _view: &wgpu::TextureView, _device: &wgpu::Device, _queue: &wgpu::Queue);
    }

    #[cfg(feature = "kms_runner")]
    pub fn run<E: Example>(_name: &str) {
        use rust_embedded_wgpu::kms;
        let mut ctx = kms::init().expect("kms init");
        let format = ctx.presenter.preferred_format();
        let mut config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: ctx.presenter.width.max(1),
            height: ctx.presenter.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![format], desired_maximum_frame_latency: 2,
        };
        let mut app = E::init(&config, &ctx.adapter, &ctx.device, &ctx.queue);
        loop {
            // Borrow device/queue separately from presenter to satisfy borrow checker
            let view = unsafe {
                // SAFETY: target_view lifetime is tied to presenter which lives for the whole loop
                &*(&ctx.presenter.target_view as *const wgpu::TextureView)
            };
            app.render(view, &ctx.device, &ctx.queue);
            // Copy from GPU target to KMS dumb buffer and page-flip
            ctx.presenter
                .present_only(&ctx.device, &ctx.queue)
                .expect("present");
        }
    }

    #[cfg(not(feature = "kms_runner"))]
    pub fn run<E: Example>(name: &str) {
        use winit::{
            event::*,
            event_loop::{ControlFlow, EventLoop},
            window::WindowBuilder,
        };
        // winit 0.29: run() closure takes (Event, &EventLoopWindowTarget) -- no ControlFlow param
        let event_loop = EventLoop::new().unwrap();
        let window = WindowBuilder::new().with_title(name).build(&event_loop).unwrap();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = instance.create_surface(&window).unwrap();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        })).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some(name),
            required_features: E::optional_features(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        })).unwrap();
        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb))
            .unwrap_or(caps.formats[0]);
        let present_mode = caps.present_modes.iter().copied()
            .find(|&m| m == wgpu::PresentMode::Fifo)
            .unwrap_or(caps.present_modes[0]);
        let alpha_mode = caps.alpha_modes.iter().copied()
            .find(|&a| a == wgpu::CompositeAlphaMode::Opaque)
            .unwrap_or(caps.alpha_modes[0]);
        let mut config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format, width: size.width.max(1), height: size.height.max(1),
            present_mode, alpha_mode,
            view_formats: vec![format],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);
        let mut app = E::init(&config, &adapter, &device, &queue);
        // winit 0.29 closure: (Event<()>, &EventLoopWindowTarget<()>)
        event_loop.run(|event, elwt| {
            elwt.set_control_flow(ControlFlow::Poll);
            match event {
                Event::WindowEvent { event, .. } => match event {
                    WindowEvent::CloseRequested => elwt.exit(),
                    WindowEvent::Resized(sz) => {
                        config.width = sz.width.max(1);
                        config.height = sz.height.max(1);
                        surface.configure(&device, &config);
                        app.resize(&config, &device, &queue);
                    }
                    WindowEvent::ScaleFactorChanged { .. } => {
                        let sz = window.inner_size();
                        config.width = sz.width.max(1);
                        config.height = sz.height.max(1);
                        surface.configure(&device, &config);
                        app.resize(&config, &device, &queue);
                    }
                    WindowEvent::RedrawRequested => {
                        match surface.get_current_texture() {
                            Ok(frame) => {
                                let view = frame.texture.create_view(
                                    &wgpu::TextureViewDescriptor::default());
                                app.render(&view, &device, &queue);
                                frame.present();
                            }
                            Err(wgpu::SurfaceError::Lost)
                            | Err(wgpu::SurfaceError::Outdated) => {
                                surface.configure(&device, &config)
                            }
                            Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                            Err(_) => {}
                        }
                    }
                    other => app.update(other),
                },
                Event::AboutToWait => window.request_redraw(),
                _ => {}
            }
        }).unwrap();
    }
}

// Vendor upstream module and shader. Paths are relative to this file.
#[path = "upstream/cube/mod.rs"]
mod upstream;

// Entry just delegates to upstream's main (which calls crate::framework::run::<Example>(...))
fn main() { upstream::main() }
