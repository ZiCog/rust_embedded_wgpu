// Re-vendored hello_triangle with a tiny cfg gate: use winit by default, or our KMS lib when
// built with --features kms_runner.
// This keeps the pipeline/shader logic in-file (close to upstream), and only switches the runner.

use anyhow::Result;
use anyhow::Context;

#[derive(Clone, Copy, Debug)]
pub struct ExampleSize { pub width: u32, pub height: u32 }

pub struct HelloTriangle {
    pipeline: wgpu::RenderPipeline,
    _shader: wgpu::ShaderModule,
    _format: wgpu::TextureFormat,
    _size: ExampleSize,
}

impl HelloTriangle {
    pub fn new(
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        size: ExampleSize,
    ) -> Result<Self> {
        let shader_src = r#"
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 0.5),
        vec2<f32>(-0.5, -0.5),
        vec2<f32>(0.5, -0.5),
    );
    let p = pos[vertex_index];
    return vec4<f32>(p, 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(0.9, 0.4, 0.2, 1.0);
}
"#;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("hello_triangle_shader"),
            source: wgpu::ShaderSource::Wgsl(shader_src.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hello_triangle_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("hello_triangle_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        Ok(Self { pipeline, _shader: shader, _format: format, _size: size })
    }

    pub fn render(&mut self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) -> Result<()> {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hello_triangle_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.draw(0..3, 0..1);
        drop(pass);
        Ok(())
    }
}

#[cfg(feature = "kms_runner")]
fn main() -> Result<()> {
    use rust_embedded_wgpu::kms::{self, frame_loop};
    let ctx = kms::init()?;
    let mut app = HelloTriangle::new(
        &ctx.device,
        &ctx.queue,
        ctx.presenter.preferred_format(),
        ExampleSize { width: ctx.presenter.width, height: ctx.presenter.height },
    )?;
    frame_loop(ctx, move |_device, _queue, encoder, view| app.render(encoder, view))
}

#[cfg(not(feature = "kms_runner"))]
fn main() -> Result<()> {
    use winit::{event::*, event_loop::{ControlFlow, EventLoop}, window::WindowBuilder};

    let event_loop = EventLoop::new().unwrap();
    let window = WindowBuilder::new().with_title("hello_triangle").build(&event_loop).unwrap();

    // Instance/surface/adapter/device
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::all(), ..Default::default() });
    let surface = instance.create_surface(&window).unwrap();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    })).context("request_adapter")?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("hello_triangle_device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    })).context("request_device")?;

    // Surface config
    let size = window.inner_size();
    let caps = surface.get_capabilities(&adapter);
    let format = caps.formats.iter().copied().find(|f| matches!(f, wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb)).unwrap_or(caps.formats[0]);
    let present_mode = caps.present_modes.iter().copied().find(|&m| m == wgpu::PresentMode::Fifo).unwrap_or(caps.present_modes[0]);
    let alpha_mode = caps.alpha_modes.iter().copied().find(|&a| a == wgpu::CompositeAlphaMode::Opaque).unwrap_or(caps.alpha_modes[0]);
    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode,
        alpha_mode,
        view_formats: vec![format],
            desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    // App
    let mut app = HelloTriangle::new(
        &device,
        &queue,
        format,
        ExampleSize { width: config.width, height: config.height },
    )?;

    event_loop.run(|event, elwt| {
        elwt.set_control_flow(ControlFlow::Poll);
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::Resized(sz) => {
                    config.width = sz.width.max(1);
                    config.height = sz.height.max(1);
                    surface.configure(&device, &config);
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    let sz = window.inner_size();
                    config.width = sz.width.max(1);
                    config.height = sz.height.max(1);
                    surface.configure(&device, &config);
                }
                WindowEvent::RedrawRequested => {
                    match surface.get_current_texture() {
                        Ok(frame) => {
                            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });
                            let _ = app.render(&mut encoder, &view);
                            queue.submit([encoder.finish()]);
                            frame.present();
                        }
                        Err(wgpu::SurfaceError::Lost) | Err(wgpu::SurfaceError::Outdated) => {
                            surface.configure(&device, &config);
                        }
                        Err(wgpu::SurfaceError::OutOfMemory) => {
                            elwt.exit();
                        }
                        Err(_) => {}
                    }
                }
                _ => {}
            },
            Event::AboutToWait => window.request_redraw(),
            _ => {}
        }
    }).unwrap();
    Ok(())
}
