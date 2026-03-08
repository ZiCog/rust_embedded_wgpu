// Re-vendored cube example with cfg-gated runner: winit by default, KMS with --features kms_runner.
// Minimal, self-contained pipeline+WGSL kept inline (close to upstream). Uses glam for matrices.

use anyhow::{Context, Result};
use std::time::Instant;
use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

#[derive(Clone, Copy, Debug)]
pub struct ExampleSize { pub width: u32, pub height: u32 }

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms { mvp: [[f32; 4]; 4] }

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex { pos: [f32; 3], color: [f32; 3] }

impl Vertex {
    const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Vertex>() as u64, step_mode: wgpu::VertexStepMode::Vertex, attributes: &Self::ATTRS }
    }
}

pub struct CubeApp {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    ubo: wgpu::Buffer,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    index_count: u32,
    depth: wgpu::Texture,
    depth_view: wgpu::TextureView,
    format: wgpu::TextureFormat,
    size: ExampleSize,
    // Animation timing
    t_run: f32,
    last_tick: Instant,
    paused: bool,
}

impl CubeApp {
    pub fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat, size: ExampleSize) -> Result<Self> {
        let shader_src = r#"
struct Uniforms { mvp: mat4x4<f32> }
@group(0) @binding(0) var<uniform> U: Uniforms;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec3<f32> };

@vertex
fn vs_main(@location(0) pos: vec3<f32>, @location(1) color: vec3<f32>) -> VsOut {
    var out: VsOut;
    out.pos = U.mvp * vec4<f32>(pos, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
"#;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("cube_shader"), source: wgpu::ShaderSource::Wgsl(shader_src.into()) });

        // Geometry (unit cube with vertex colors)
        let verts: [Vertex; 8] = [
            Vertex { pos: [-1.0, -1.0, -1.0], color: [1.0, 0.0, 0.0] },
            Vertex { pos: [ 1.0, -1.0, -1.0], color: [0.0, 1.0, 0.0] },
            Vertex { pos: [ 1.0,  1.0, -1.0], color: [0.0, 0.0, 1.0] },
            Vertex { pos: [-1.0,  1.0, -1.0], color: [1.0, 1.0, 0.0] },
            Vertex { pos: [-1.0, -1.0,  1.0], color: [1.0, 0.0, 1.0] },
            Vertex { pos: [ 1.0, -1.0,  1.0], color: [0.0, 1.0, 1.0] },
            Vertex { pos: [ 1.0,  1.0,  1.0], color: [1.0, 1.0, 1.0] },
            Vertex { pos: [-1.0,  1.0,  1.0], color: [0.2, 0.8, 0.4] },
        ];
        let idx: [u16; 36] = [
            0,1,2, 2,3,0, // back
            4,6,5, 6,4,7, // front
            0,4,5, 5,1,0, // bottom
            3,2,6, 6,7,3, // top
            1,5,6, 6,2,1, // right
            0,3,7, 7,4,0, // left
        ];
        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("cube_vbuf"), contents: bytemuck::cast_slice(&verts), usage: wgpu::BufferUsages::VERTEX });
        let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("cube_ibuf"), contents: bytemuck::cast_slice(&idx), usage: wgpu::BufferUsages::INDEX });

        // Uniforms
        let ubo = device.create_buffer(&wgpu::BufferDescriptor { label: Some("cube_ubo"), size: std::mem::size_of::<Uniforms>() as u64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cube_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::VERTEX, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None }],
        });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("cube_bg"), layout: &bgl, entries: &[wgpu::BindGroupEntry { binding: 0, resource: ubo.as_entire_binding() }] });

        // Depth
        let depth_format = wgpu::TextureFormat::Depth24Plus;
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cube_depth"),
            size: wgpu::Extent3d { width: size.width.max(1), height: size.height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: depth_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

        // Pipeline
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("cube_pl"), bind_group_layouts: &[&bgl], push_constant_ranges: &[] });
        let rp = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cube_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"), buffers: &[Vertex::layout()], compilation_options: wgpu::PipelineCompilationOptions::default() },
            fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs_main"), targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::REPLACE), write_mask: wgpu::ColorWrites::ALL })], compilation_options: wgpu::PipelineCompilationOptions::default() }),
            primitive: wgpu::PrimitiveState { cull_mode: Some(wgpu::Face::Back), ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState { format: depth_format, depth_write_enabled: true, depth_compare: wgpu::CompareFunction::Less, stencil: wgpu::StencilState::default(), bias: wgpu::DepthBiasState::default() }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self { pipeline: rp, bind_group: bg, ubo, vbuf, ibuf, index_count: idx.len() as u32, depth, depth_view, format, size, t_run: 0.0, last_tick: Instant::now(), paused: false })
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32();
        self.last_tick = now;
        if !self.paused { self.t_run += dt; }
    }

    pub fn toggle_pause(&mut self) { self.paused = !self.paused; }

    fn mvp(&self) -> Uniforms {
        let t = self.t_run;
        let aspect = (self.size.width.max(1) as f32) / (self.size.height.max(1) as f32);
        let proj = Mat4::perspective_rh_gl(45f32.to_radians(), aspect, 0.1, 100.0);
        let view = Mat4::look_at_rh(Vec3::new(2.5* t.cos(), 2.0, 2.5* t.sin()), Vec3::ZERO, Vec3::Y);
        let model = Mat4::from_rotation_y(t*0.8) * Mat4::from_rotation_x(t*0.4);
        Uniforms { mvp: (proj * view * model).to_cols_array_2d() }
    }

    fn ensure_depth(&mut self, device: &wgpu::Device, new_size: ExampleSize) {
        if new_size.width == self.size.width && new_size.height == self.size.height { return; }
        self.size = new_size;
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cube_depth"),
            size: wgpu::Extent3d { width: self.size.width.max(1), height: self.size.height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24Plus,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        self.depth = depth;
    }

    pub fn render(&mut self, _device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) -> Result<()> {
        // Advance animation timer and update UBO
        self.tick();
        let u = self.mvp();
        queue.write_buffer(&self.ubo, 0, bytemuck::bytes_of(&u));

        // Render pass
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("cube_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.02, g: 0.02, b: 0.03, a: 1.0 }), store: wgpu::StoreOp::Store } })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment { view: &self.depth_view, depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }), stencil_ops: None }),
            occlusion_query_set: None, timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vbuf.slice(..));
        pass.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed(0..self.index_count, 0, 0..1);
        drop(pass);
        Ok(())
    }
}

#[cfg(feature = "kms_runner")]
fn main() -> Result<()> {
    use rust_embedded_wgpu::kms::{self, frame_loop};
    let ctx = kms::init()?;
    let mut app = CubeApp::new(&ctx.device, &ctx.queue, ctx.presenter.preferred_format(), ExampleSize { width: ctx.presenter.width, height: ctx.presenter.height })?;
    frame_loop(ctx, move |device, queue, encoder, view| app.render(device, queue, encoder, view))
}


#[cfg(not(feature = "kms_runner"))]
fn main() -> Result<()> {
    use winit::{
        event::*,
        event_loop::{ControlFlow, EventLoop},
        window::WindowBuilder,
    };

    let event_loop = EventLoop::new().unwrap();
    let window = WindowBuilder::new().with_title("cube").build(&event_loop).unwrap();

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    let surface = instance.create_surface(&window).unwrap();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    })).context("request_adapter")?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("cube_device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
        ..Default::default()
    })).context("request_device")?;

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
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode,
        alpha_mode,
        view_formats: vec![format],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    let mut app = CubeApp::new(
        &device, &queue, format,
        ExampleSize { width: config.width, height: config.height },
    )?;

    event_loop.run(|event, elwt| {
        elwt.set_control_flow(ControlFlow::Poll);
        match event {
                Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::KeyboardInput { event, .. } => {
                    use winit::event::ElementState;
                    use winit::keyboard::{Key, NamedKey};
                    if event.state == ElementState::Pressed {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => elwt.exit(),
                            Key::Named(NamedKey::Space) => app.toggle_pause(),
                            Key::Character(s) if s == " " => app.toggle_pause(),
                            _ => {}
                        }
                    }
                },
                WindowEvent::Resized(sz) => {
                    config.width = sz.width.max(1);
                    config.height = sz.height.max(1);
                    surface.configure(&device, &config);
                    app.ensure_depth(&device, ExampleSize { width: config.width, height: config.height });
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    let sz = window.inner_size();
                    config.width = sz.width.max(1);
                    config.height = sz.height.max(1);
                    surface.configure(&device, &config);
                    app.ensure_depth(&device, ExampleSize { width: config.width, height: config.height });
                }
                WindowEvent::RedrawRequested => {
                    match surface.get_current_texture() {
                        Ok(frame) => {
                            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                            let mut encoder = device.create_command_encoder(
                                &wgpu::CommandEncoderDescriptor { label: Some("cube_enc") });
                            let _ = app.render(&device, &queue, &mut encoder, &view);
                            queue.submit([encoder.finish()]);
                            frame.present();
                        }
                        Err(wgpu::SurfaceError::Lost) | Err(wgpu::SurfaceError::Outdated) => {
                            surface.configure(&device, &config);
                        }
                        Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
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
