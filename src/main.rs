use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    #[cfg(all(target_os = "linux", feature = "wgpu_drm"))]
    {
        return linux::run().await;
    }

    #[cfg(not(all(target_os = "linux", feature = "wgpu_drm")))]
    {
        eprintln!(
            "This scaffold targets Linux DRM/KMS with wgpu. Build on a Linux device with:\n  cargo run --release --features wgpu_drm\n(Optional ESC via evdev: add --features esc_evdev)"
        );
        Ok(())
    }
}

#[cfg(all(target_os = "linux", feature = "wgpu_drm"))]
mod linux {
    use anyhow::{bail, Context, Result};
    use std::{
        fs::File,
        os::fd::{AsFd, AsRawFd, BorrowedFd},
        time::{Duration, Instant},
    };

    use drm::control::{self as ctrl, Device as ControlDevice, ModeTypeFlags};

    pub(crate) struct Card(File);

    impl Card {
        fn as_borrowed_fd(&self) -> BorrowedFd<'_> { self.0.as_fd() }
        fn raw_fd(&self) -> i32 { self.0.as_raw_fd() }
    }

    // Implement DRM device traits for our file-backed Card
    impl drm::Device for Card {}
    impl ctrl::Device for Card {}

    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> { self.0.as_fd() }
    }

    #[derive(Debug, Clone, Copy)]
    pub(crate) struct DrmPick {
        pub(crate) connector_id: u32,
        pub(crate) width: u32,
        pub(crate) height: u32,
        pub(crate) refresh_millihz: u32,
    }

    fn open_card_and_pick() -> Result<(Card, DrmPick)> {
        for i in 0..=9 {
            let path = format!("/dev/dri/card{}", i);
            let Ok(f) = File::options().read(true).write(true).open(&path) else { continue };
            let card = Card(f);

            let Ok(res) = card.resource_handles() else { continue };
            for &conn in res.connectors().iter() {
                let Ok(info) = card.get_connector(conn, false) else { continue };
                if info.state() != ctrl::connector::State::Connected { continue; }
                let modes = info.modes();
                if modes.is_empty() { continue; }
                let preferred = modes
                    .iter()
                    .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                    .copied()
                    .unwrap_or(modes[0]);

                let (w, h) = preferred.size();
                let vrefresh_hz = preferred.vrefresh();
                let pick = DrmPick {
                    connector_id: conn.into(),
                    width: w as u32,
                    height: h as u32,
                    refresh_millihz: (vrefresh_hz as u32) * 1000,
                };
                return Ok((card, pick));
            }
            // If no connected connector on this card, try next card
        }
        bail!("No DRM card with a connected connector/mode found");
    }

    // Optional: spawn an ESC listener via evdev (requires feature "esc_evdev")
    #[cfg(feature = "esc_evdev")]
    fn spawn_esc_listener() -> tokio::sync::oneshot::Receiver<()> {
        use evdev::{Device, Key};
        use std::thread;
        use tokio::sync::oneshot;

        let (tx, rx) = oneshot::channel();
        thread::spawn(move || {
            let mut devices: Vec<Device> = evdev::enumerate()
                .filter_map(|(p, _)| Device::open(p).ok())
                .collect();

            'outer: loop {
                for dev in devices.iter_mut() {
                    if let Ok(events) = dev.fetch_events() {
                        for e in events {
                            if let Some(code) = e.code() {
                                if code == Key::ESC && e.value() == 1 {
                                    let _ = tx.send(());
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        rx
    }

    pub async fn run() -> Result<()> {
        // 1) DRM: scan cards and pick first connected connector/mode
        let (card, pick) = open_card_and_pick().context("pick DRM card/connector/mode")?;

        // 2) wgpu instance (Vulkan backend preferred)
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            flags: wgpu::InstanceFlags::empty(),
            ..Default::default()
        });

        // 3) Create a DRM surface (unsafe; requires valid params, and the FD must outlive the surface)
        let surface = match unsafe {
            use wgpu::SurfaceTargetUnsafe;
            instance.create_surface_unsafe(SurfaceTargetUnsafe::Drm {
                fd: card.raw_fd(),
                plane: 0, // primary plane
                connector_id: pick.connector_id,
                width: pick.width,
                height: pick.height,
                refresh_rate: pick.refresh_millihz,
            })
        } {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wgpu DRM surface create failed: {:?}", e);
                #[cfg(feature = "kms_cpu_scanout")]
                { return crate::cpu_scanout::run(&card, pick).await; }
                #[allow(unreachable_code)]
                return Err(anyhow::anyhow!("wgpu DRM surface create failed and kms_cpu_scanout not enabled"));
            }
        };

        // 4) Request adapter/device bound to this surface
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("No suitable Vulkan adapter for DRM surface")?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            })
            .await?;

        // 5) Configure surface
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb))
            .unwrap_or_else(|| caps.formats[0]);
        let present_mode = caps
            .present_modes
            .iter()
            .copied()
            .find(|&m| m == wgpu::PresentMode::Fifo)
            .unwrap_or(caps.present_modes[0]);
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|&a| a == wgpu::CompositeAlphaMode::Opaque)
            .unwrap_or(caps.alpha_modes[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: pick.width,
            height: pick.height,
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // 6) Pipeline: color-changing triangle with a time uniform
        #[repr(C)]
        #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
        struct TimeUniform { t: f32 }

        let time_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("time_ubo"),
            size: std::mem::size_of::<TimeUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: time_buf.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("triangle_wgsl"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(
                r#"
struct Uniforms { time: f32; };
@group(0) @binding(0) var<uniform> U: Uniforms;

struct VSOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) color: vec3<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VSOut {
  var positions = array<vec2<f32>, 3>(
    vec2<f32>(0.0,  0.75),
    vec2<f32>(-0.75, -0.75),
    vec2<f32>(0.75, -0.75)
  );
  var colors = array<vec3<f32>, 3>(
    vec3<f32>(1.0, 0.0, 0.0),
    vec3<f32>(0.0, 1.0, 0.0),
    vec3<f32>(0.0, 0.0, 1.0)
  );
  var out: VSOut;
  out.pos = vec4<f32>(positions[vid], 0.0, 1.0);
  let t = U.time;
  let k = 0.5 + 0.5 * cos(t);
  out.color = mix(colors[vid], vec3<f32>(k, 1.0 - k, 0.5 + 0.5 * sin(t)), 0.5);
  return out;
}

@fragment
fn fs_main(in: VSOut) -> @location(0) vec4<f32> {
  return vec4<f32>(in.color, 1.0);
}
"#,
            )),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("triangle_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // 7) Async render loop with tokio; update time uniform each frame
        let start = Instant::now();
        let mut ticker = tokio::time::interval(Duration::from_millis(16));

        #[cfg(feature = "esc_evdev")]
        let mut esc_rx = spawn_esc_listener();

        loop {
            #[cfg(feature = "esc_evdev")]
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = &mut esc_rx => break,
                _ = ticker.tick() => {}
            }
            #[cfg(not(feature = "esc_evdev"))]
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = ticker.tick() => {}
            }

            // Update time uniform
            let t = start.elapsed().as_secs_f32();
            let u = TimeUniform { t };
            let data = bytemuck::bytes_of(&u);
            queue.write_buffer(&time_buf, 0, data);

            // Acquire frame and render
            let frame = match surface.get_current_texture() {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("surface error: {e:?}; reconfiguring");
                    surface.configure(&device, &config);
                    continue;
                }
            };
            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });
            {
                let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("rp"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
                rp.set_pipeline(&pipeline);
                rp.set_bind_group(0, &bind_group, &[]);
                rp.draw(0..3, 0..1);
            }
            queue.submit([encoder.finish()]);
            frame.present();
        }

        Ok(())
    }
}


// --- BEGIN kms_cpu_scanout ---
#[cfg(all(target_os = "linux", feature = "kms_cpu_scanout"))]
mod cpu_scanout {
    use super::linux::{Card, DrmPick};
    use anyhow::{anyhow, Context, Result};
    use drm::buffer::Buffer as _; // bring pitch()/size() into scope for DumbBuffer
    use drm::control::{self as ctrl, Device as ControlDevice, Event, PageFlipFlags};
    use std::num::{NonZeroU32, NonZeroU64};
    use std::time::{Duration, Instant};

    // Offscreen wgpu renderer that draws to an RGBA8 texture and stages to a CPU-readable buffer.
    struct Offscreen {
        device: wgpu::Device,
        queue: wgpu::Queue,
        pipeline: wgpu::RenderPipeline,
        time_buf: wgpu::Buffer,
        bind_group: wgpu::BindGroup,
        target: wgpu::Texture,
        target_view: wgpu::TextureView,
        width: u32,
        height: u32,
        readback: wgpu::Buffer,
        padded_bpr: u32,
        unpadded_bpr: u32,
    }

    impl Offscreen {
        async fn new(width: u32, height: u32) -> Result<Self> {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                backends: wgpu::Backends::VULKAN,
                ..Default::default()
            });
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await?;

            let (device, queue) = adapter
                .request_device(
                    &wgpu::DeviceDescriptor {
                        label: Some("offscreen_device"),
                        required_features: wgpu::Features::empty(),
                        required_limits: wgpu::Limits::default(),
                        ..Default::default()
                    },
                )
                .await?;

            // Uniform buffer for time (seconds)
            #[repr(C)]
            #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
            struct TimeUbo {
                time: f32,
            }
            let time_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("time_ubo"),
                size: std::mem::size_of::<TimeUbo>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(std::mem::size_of::<TimeUbo>() as u64),
                    },
                    count: None,
                }],
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("bg"),
                layout: &bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: time_buf.as_entire_binding(),
                }],
            });

            // Simple triangle shader (same animation idea as main path)
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("triangle_wgsl"),
                source: wgpu::ShaderSource::Wgsl(
                    r#"
                    struct TimeUbo { time: f32 };
                    @group(0) @binding(0) var<uniform> u_time: TimeUbo;

                    struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) c: vec3<f32> };

                    @vertex
                    fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
                        var p = array<vec2<f32>, 3>(
                            vec2<f32>( 0.0,  0.75),
                            vec2<f32>(-0.75, -0.75),
                            vec2<f32>( 0.75, -0.75)
                        );
                        let pos = p[vi];
                        let t = u_time.time;
                        var out: VsOut;
                        out.pos = vec4<f32>(pos, 0.0, 1.0);
                        out.c = vec3<f32>(
                            0.5 + 0.5 * sin(t * 0.7),
                            0.5 + 0.5 * cos(t * 1.1),
                            0.5 + 0.5 * sin(t * 0.9)
                        );
                        return out;
                    }

                    @fragment
                    fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
                        return vec4<f32>(in.c, 1.0);
                    }
                "#
                    .into(),
                ),
            });

            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pipeline_layout"),
                bind_group_layouts: &[&bgl],
                push_constant_ranges: &[],
            });

            let format = wgpu::TextureFormat::Rgba8Unorm;

            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("triangle_pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });

            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("offscreen_target"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

            let bytes_per_pixel = 4u32;
            let unpadded_bpr = width * bytes_per_pixel;
            let padded_bpr = ((unpadded_bpr + 255) / 256) * 256;
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: padded_bpr as u64 * height as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            Ok(Self {
                device,
                queue,
                pipeline,
                time_buf,
                bind_group,
                target,
                target_view,
                width,
                height,
                readback,
                padded_bpr,
                unpadded_bpr,
            })
        }

        fn render_and_read_rgba(&self, t: f32) -> Result<Vec<u8>> {
            // Update time uniform
            #[repr(C)]
            #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
            struct TimeUbo { time: f32 }
            let data = TimeUbo { time: t };
            self.queue.write_buffer(&self.time_buf, 0, bytemuck::bytes_of(&data));

            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });

            // Render triangle to offscreen target
            {
                let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("rpass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.target_view,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                rpass.set_pipeline(&self.pipeline);
                rpass.set_bind_group(0, &self.bind_group, &[]);
                rpass.draw(0..3, 0..1);
            }

            // Copy texture -> readback buffer with padded bytes_per_row
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.target,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(self.padded_bpr),
                        rows_per_image: Some(self.height),
                    },
                },
                wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
            );

            self.queue.submit(std::iter::once(encoder.finish()));

            // Map and read back
            let slice = self.readback.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            let _ = self.device.poll(wgpu::PollType::Wait);
            let data = slice.get_mapped_range();

            // Pack into tightly packed RGBA rows for convenience.
            let mut out = vec![0u8; (self.unpadded_bpr as usize) * (self.height as usize)];
            for y in 0..self.height as usize {
                let src = &data[y * (self.padded_bpr as usize)..][..(self.unpadded_bpr as usize)];
                let dst = &mut out[y * (self.unpadded_bpr as usize)..][..(self.unpadded_bpr as usize)];
                dst.copy_from_slice(src);
            }
            drop(data);
            self.readback.unmap();
            Ok(out)
        }
    }

    struct DbBuf {
        dbuf: ctrl::dumbbuffer::DumbBuffer,
        fb: ctrl::framebuffer::Handle,
    }

    struct FbRes {
        crtc: ctrl::crtc::Handle,
        conn: ctrl::connector::Handle,
        mode: ctrl::Mode,
        bufs: [DbBuf; 2],
        width: u32,
        height: u32,
    }

    /// Tear-free double-buffered KMS scanout: render with wgpu offscreen RGBA8, convert to XRGB8888,
    /// blit into the back dumb buffer, then page_flip with EVENT and wait for vblank.
    pub async fn run(card: &Card, pick: DrmPick) -> Result<()> {
        let mut fb = setup_kms_double_fb(card, pick).context("setup KMS double dumb FBs")?;
        let mut back = 1usize; // we set CRTC to bufs[0]; start drawing into bufs[1]

        let mut gfx = Offscreen::new(fb.width, fb.height).await.context("setup wgpu offscreen")?;
        let start = Instant::now();

        loop {
            let t = start.elapsed().as_secs_f32();
            let rgba = gfx.render_and_read_rgba(t)?; // tightly packed RGBA rows

            // Map back buffer and blit RGBA->XRGB row by row
            let (w, h) = (fb.width as usize, fb.height as usize);
            let pitch = fb.bufs[back].dbuf.pitch() as usize;
            {
                let mut map = card
                    .map_dumb_buffer(&mut fb.bufs[back].dbuf)
                    .map_err(|e| annotate_eacces(e, "map_dumb_buffer"))?;
                for y in 0..h {
                    let src = &rgba[y * (w * 4)..][..(w * 4)];
                    let row = &mut map[y * pitch..][..pitch];
                    // Convert RGBA -> XRGB8888 (little-endian bytes: B,G,R,X)
                    for x in 0..w {
                        let si = x * 4;
                        let r = src[si + 0];
                        let g = src[si + 1];
                        let b = src[si + 2];
                        let di = x * 4;
                        row[di + 0] = b;
                        row[di + 1] = g;
                        row[di + 2] = r;
                        row[di + 3] = 0xFF;
                    }
                }
            }

            // Queue page flip to the back buffer on vblank and wait for the flip event.
            card
                .page_flip(
                    fb.crtc,
                    fb.bufs[back].fb,
                    PageFlipFlags::EVENT,
                    None,
                )
                .map_err(|e| annotate_eacces(e, "page_flip"))?;

            // Block for the flip event on our CRTC
            loop {
                let evs = card
                    .receive_events()
                    .map_err(|e| annotate_eacces(e, "receive_events"))?;
                let mut done = false;
                for ev in evs {
                    if let Event::PageFlip(e) = ev {
                        if e.crtc == fb.crtc {
                            done = true;
                        }
                    }
                }
                if done {
                    break;
                }
                // Be polite if no events were returned spuriously
                std::thread::sleep(Duration::from_millis(1));
            }

            // Swap indices: the buffer we just flipped becomes the new front; render into the other one next.
            back ^= 1;
        }
    }

    fn setup_kms_double_fb(card: &Card, pick: DrmPick) -> Result<FbRes> {
        // Pick a connected connector with a mode, and its CRTC via the first encoder.
        let res = card.resource_handles().context("resource_handles")?;
        let (conn, conn_info) = res
            .connectors()
            .iter()
            .filter_map(|&c| card.get_connector(c, true).ok().map(|i| (c, i)))
            .find(|(_, i)| i.state() == ctrl::connector::State::Connected && !i.modes().is_empty())
            .ok_or_else(|| anyhow!("no connected connector found"))?;

        let enc = *conn_info
            .encoders()
            .get(0)
            .ok_or_else(|| anyhow!("no encoder for connector"))?;
        let enc_info = card.get_encoder(enc).context("get_encoder")?;
        let crtc = enc_info.crtc().ok_or_else(|| anyhow!("no crtc available"))?;

        let mode = *conn_info
            .modes()
            .get(0)
            .ok_or_else(|| anyhow!("no modes"))?;

        let width = pick.width.max(1);
        let height = pick.height.max(1);

        let mut make_buf = |label: &str| -> Result<DbBuf> {
            let mut dbuf = card
                .create_dumb_buffer((width, height), drm::buffer::DrmFourcc::Xrgb8888, 32)
                .map_err(|e| annotate_eacces(e, label))?;
            // Clear visible memory to something sane
            {
                let mut map = card
                    .map_dumb_buffer(&mut dbuf)
                    .map_err(|e| annotate_eacces(e, "map_dumb_buffer"))?;
                for px in map.chunks_exact_mut(4) {
                    px[0] = 0x10; // B
                    px[1] = 0x10; // G
                    px[2] = 0x10; // R
                    px[3] = 0xFF; // X
                }
            }
            let fb = card
                .add_framebuffer(&dbuf, 24, 32)
                .map_err(|e| annotate_eacces(e, "add_framebuffer"))?;
            Ok(DbBuf { dbuf, fb })
        };

        let b0 = make_buf("create_dumb_buffer[0]")?;
        let b1 = make_buf("create_dumb_buffer[1]")?;

        // Set CRTC to scan out from the first framebuffer initially.
        card
            .set_crtc(crtc, Some(b0.fb), (0, 0), &[conn], Some(mode))
            .map_err(|e| annotate_eacces(e, "set_crtc"))?;

        Ok(FbRes { crtc, conn, mode, bufs: [b0, b1], width, height })
    }

    // If we hit EACCES (permission denied), annotate with actionable guidance for DRM master/seat.
    fn annotate_eacces(err: std::io::Error, where_: &str) -> anyhow::Error {
        use std::io::ErrorKind;
        if err.kind() == ErrorKind::PermissionDenied {
            anyhow!(
                "{}: Permission denied. Run on a local VT/seat with DRM master (not SSH), or stop the display manager.\n\
                 Tips:\n\
                 - Switch to a text VT (e.g., Ctrl+Alt+F2 on Jetson), log in, and run the binary there.\n\
                 - Ensure your user is in the 'video' group (for /dev/dri/card*).\n\
                 - If using seatd/logind, launch from a seat-managed session so the process can acquire DRM master.",
                where_
            )
        } else {
            anyhow!(err).context(where_.to_string())
        }
    }
}
// --- END kms_cpu_scanout ---
