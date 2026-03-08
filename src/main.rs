use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Initialize logging once (respects RUST_LOG); ignore error if already inited.
    let _ = env_logger::try_init();
    #[cfg(all(target_os = "linux", feature = "wgpu_drm"))]
    {
        return linux::run().await;
    }

    #[cfg(not(all(target_os = "linux", feature = "wgpu_drm")))]
    {
        eprintln!(
            "This project targets Linux DRM/KMS with wgpu. Build/run on a Linux device with:\n  cargo run --release --features wgpu_drm,kms_cpu_scanout\n(Optional ESC via evdev: add --features esc_evdev)"
        );
        Ok(())
    }
}

#[cfg(all(target_os = "linux", feature = "wgpu_drm"))]
mod linux {
    use anyhow::{anyhow, bail, Context, Result};
    use std::{
        fs::File,
        io::ErrorKind,
        os::fd::{AsFd, AsRawFd, BorrowedFd},
        time::{Duration, Instant},
    };

    use drm::control::{self as ctrl, Device as ControlDevice, Event, ModeTypeFlags, PageFlipFlags};
    use drm::buffer::Buffer as _; // needed for .pitch()
    use std::io::Write;
    use std::path::PathBuf;

    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
    pub(crate) struct TimeUbo {
        pub(crate) time: f32,
        pub(crate) _pad: [f32; 3],
    }

    // ----- DRM card wrapper -----
    pub(crate) struct Card(File);
    impl drm::Device for Card {}
    impl ctrl::Device for Card {}
    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl Card {
        fn raw_fd(&self) -> i32 {
            self.0.as_raw_fd()
        }
        fn try_clone(&self) -> std::io::Result<Card> {
            Ok(Card(self.0.try_clone()?))
        }
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
            let Ok(f) = File::options().read(true).write(true).open(&path) else {
                continue;
            };
            let card = Card(f);

            let Ok(res) = card.resource_handles() else { continue };
            for &conn in res.connectors().iter() {
                let Ok(info) = card.get_connector(conn, false) else { continue };
                if info.state() != ctrl::connector::State::Connected { continue }
                let modes = info.modes();
                if modes.is_empty() { continue }
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
        }
        bail!("No DRM card with a connected connector/mode found");
    }

    // Optional: spawn an ESC listener via evdev (requires feature "esc_evdev")
    #[cfg(feature = "esc_evdev")]
    fn spawn_esc_listener() -> tokio::sync::oneshot::Receiver<()> {
        use evdev::{Device, Key};
        use std::thread;
        use tokio::sync::oneshot;

        let (tx, rx) = oneshot::channel::<()>();
        thread::spawn(move || {
            let mut devices: Vec<Device> = evdev::enumerate()
                .filter_map(|(p, _)| Device::open(p).ok())
                .collect();
            'outer: loop {
                for dev in devices.iter_mut() {
                    if let Ok(events) = dev.fetch_events() {
                        for e in events {
                            if let evdev::InputEventKind::Key(key) = e.kind() {
                                if key == Key::KEY_ESC && e.value() == 1 {
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

    // ----- Renderer (single, shared) -----
    struct Renderer {
        pipeline: wgpu::RenderPipeline,
        time_buf: wgpu::Buffer,
        bind_group: wgpu::BindGroup,
    }
    impl Renderer {
        fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
            // time UBO layout
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
                        min_binding_size: std::num::NonZeroU64::new(
                            std::mem::size_of::<TimeUbo>() as u64,
                        ),
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
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pipeline_layout"),
                bind_group_layouts: &[&bgl],
                push_constant_ranges: &[],
            });
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("triangle_wgsl"),
                source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                    "../shaders/triangle.wgsl",
                ))),
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
                        format,
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
            Self { pipeline, time_buf, bind_group }
        }
        fn update_time(&self, queue: &wgpu::Queue, t: f32) {
            let u = TimeUbo { time: t, _pad: [0.0; 3] };
            queue.write_buffer(&self.time_buf, 0, bytemuck::bytes_of(&u));
        }
        fn record(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rp"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.bind_group, &[]);
            rp.draw(0..3, 0..1);
        }
    }

    // ----- Presentation abstraction -----
    pub struct FrameCtx {
        pub encoder: wgpu::CommandEncoder,
        pub view: wgpu::TextureView,
        inner: FrameInner,
    }
    enum FrameInner {
        Surface { frame: wgpu::SurfaceTexture },
        Kms,
    }

    trait PresentBackend {
        fn preferred_format(&self) -> wgpu::TextureFormat;
        fn begin_frame(&mut self, device: &wgpu::Device) -> Result<FrameCtx>;
        fn end_frame(
            &mut self,
            frame: FrameCtx,
            device: &wgpu::Device,
            queue: &wgpu::Queue,
        ) -> Result<()>;
    }

    // Surface (wgpu DRM) presenter
    struct SurfacePresenter {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    }
    impl SurfacePresenter {
        fn new(
            surface: wgpu::Surface<'static>,
            adapter: &wgpu::Adapter,
            device: &wgpu::Device,
            width: u32,
            height: u32,
        ) -> Self {
            let caps = surface.get_capabilities(adapter);
            let format = caps
                .formats
                .iter()
                .copied()
                .find(|f| matches!(
                    f,
                    wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb
                ))
                .unwrap_or(caps.formats[0]);
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
                width,
                height,
                present_mode,
                alpha_mode,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(device, &config);
            Self { surface, config }
        }
    }
    impl PresentBackend for SurfacePresenter {
        fn preferred_format(&self) -> wgpu::TextureFormat { self.config.format }
        fn begin_frame(&mut self, device: &wgpu::Device) -> Result<FrameCtx> {
            let frame = match self.surface.get_current_texture() {
                Ok(f) => f,
                Err(_) => {
                    // Try a light reconfigure on transient errors
                    self.surface.configure(device, &self.config);
                    self.surface
                        .get_current_texture()
                        .map_err(|e| anyhow!("surface acquire failed after reconfigure: {e:?}"))?
                }
            };
            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("encoder"),
            });
            Ok(FrameCtx { encoder, view, inner: FrameInner::Surface { frame } })
        }
        fn end_frame(
            &mut self,
            frame: FrameCtx,
            _device: &wgpu::Device,
            queue: &wgpu::Queue,
        ) -> Result<()> {
            let FrameCtx { encoder, inner, .. } = frame;
            queue.submit([encoder.finish()]);
            if let FrameInner::Surface { frame } = inner {
                frame.present();
            }
            Ok(())
        }
    }

    // KMS CPU presenter (double-buffered dumb scanout)
    #[cfg(feature = "kms_cpu_scanout")]
    struct KmsCpuPresenter {
        card: Card,
        // KMS resources
        crtc: ctrl::crtc::Handle,
        _conn: ctrl::connector::Handle,
        _mode: ctrl::Mode,
        bufs: [DbBuf; 2],
        width: u32,
        height: u32,
        back: usize,
        dumb_pitch: u32,
        // GPU offscreen
        target: wgpu::Texture,
        target_view: wgpu::TextureView,
        readback: wgpu::Buffer,
        copy_bpr: u32,
        dump_path: Option<PathBuf>,  // set DUMP_FRAME=/path/to/out.ppm to save first frame
        dumped: bool,
    }
    #[cfg(feature = "kms_cpu_scanout")]
    struct DbBuf {
        dbuf: ctrl::dumbbuffer::DumbBuffer,
        fb: ctrl::framebuffer::Handle,
    }
    #[cfg(feature = "kms_cpu_scanout")]
    impl KmsCpuPresenter {
        fn new(card: Card, pick: DrmPick, device: &wgpu::Device) -> Result<Self> {
            // KMS buffers
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
            let crtc = enc_info
                .crtc()
                .ok_or_else(|| anyhow!("no crtc available"))?;
            let mode = *conn_info.modes().get(0).ok_or_else(|| anyhow!("no modes"))?;
            let width = pick.width.max(1);
            let height = pick.height.max(1);

            let make_buf = |label: &str| -> Result<DbBuf> {
                let mut dbuf = card
                    .create_dumb_buffer((width, height), drm::buffer::DrmFourcc::Xrgb8888, 32)
                    .map_err(|e| annotate_eacces(e, label))?;
                // Clear
                {
                    let mut map = card
                        .map_dumb_buffer(&mut dbuf)
                        .map_err(|e| annotate_eacces(e, "map_dumb_buffer"))?;
                    for px in map.chunks_exact_mut(4) {
                        px[0] = 0x10; px[1] = 0x10; px[2] = 0x10; px[3] = 0xFF;
                    }
                }
                let fb = card
                    .add_framebuffer(&dbuf, 24, 32)
                    .map_err(|e| annotate_eacces(e, "add_framebuffer"))?;
                Ok(DbBuf { dbuf, fb })
            };
            let b0 = make_buf("create_dumb_buffer[0]")?;
            let b1 = make_buf("create_dumb_buffer[1]")?;
            card.set_crtc(crtc, Some(b0.fb), (0, 0), &[conn], Some(mode))
                .map_err(|e| annotate_eacces(e, "set_crtc"))?;
            let dumb_pitch = b0.dbuf.pitch() as u32;

            // Offscreen render target in BGRA8Unorm to match XRGB8888 byte layout (little-endian)
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("offscreen_target"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

            // Readback buffer with GPU-required 256B alignment; try to match dumb stride when possible
            let unpadded_bpr = width * 4;
            let mut copy_bpr = ((unpadded_bpr + 255) / 256) * 256;
            if dumb_pitch % 256 == 0 { copy_bpr = dumb_pitch; }
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: (copy_bpr as u64) * (height as u64),
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            Ok(Self {
                card,
                crtc,
                _conn: conn,
                _mode: mode,
                bufs: [b0, b1],
                width,
                height,
                back: 1,
                dumb_pitch,
                target,
                target_view,
                readback,
                copy_bpr,
                dump_path: std::env::var("DUMP_FRAME").ok().map(PathBuf::from),
                dumped: false,
            })
        }
        fn copy_to_readback<'a>(&'a self, encoder: &mut wgpu::CommandEncoder) {
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
                        bytes_per_row: Some(self.copy_bpr),
                        rows_per_image: Some(self.height),
                    },
                },
                wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
            );
        }
        fn blit_readback_to_dumb(&mut self, device: &wgpu::Device) -> Result<()> {
            // Ensure GPU work is done
            let _ = device.poll(wgpu::PollType::Wait);
            let slice = self.readback.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            let _ = device.poll(wgpu::PollType::Wait);
            let data = slice.get_mapped_range();

            let h = self.height as usize;
            let pitch = self.dumb_pitch as usize;
            let copy_bpr = self.copy_bpr as usize;
            let mut map = self
                .card
                .map_dumb_buffer(&mut self.bufs[self.back].dbuf)
                .map_err(|e| annotate_eacces(e, "map_dumb_buffer"))?;
            if copy_bpr == pitch {
                // Single bulk copy
                let bytes = copy_bpr * h;
                map[..bytes].copy_from_slice(&data[..bytes]);
            } else {
                for y in 0..h {
                    let src = &data[y * copy_bpr..][..(self.width as usize * 4)];
                    let row = &mut map[y * pitch..][..(self.width as usize * 4)];
                    row.copy_from_slice(src);
                }
            }
            drop(map);
            drop(data);
            self.readback.unmap();

            // Optional first-frame PPM dump for remote display verification
            if !self.dumped {
                if let Some(ref path) = self.dump_path {
                    let w = self.width as usize;
                    let h = self.height as usize;
                    let pitch = self.dumb_pitch as usize;
                    if let Ok(mut file) = std::fs::File::create(path) {
                        let _ = writeln!(file, "P6");
                        let _ = writeln!(file, "{} {}", w, h);
                        let _ = writeln!(file, "255");
                        // Read the buffer we just rendered into (self.back) -> RGB for PPM
                        if let Ok(map) = self.card.map_dumb_buffer(&mut self.bufs[self.back].dbuf) {
                            // Log first pixel so we can tell if GPU rendered anything
                            let b0 = map[0]; let g0 = map[1]; let r0 = map[2];
                            eprintln!("Dump pixel[0,0]: R={} G={} B={}", r0, g0, b0);
                            for y in 0..h {
                                let row = &map[y * pitch..][..w * 4];
                                for x in 0..w {
                                    // XRGB8888 LE: byte0=B, byte1=G, byte2=R, byte3=X
                                    let b = row[x * 4];
                                    let g = row[x * 4 + 1];
                                    let r = row[x * 4 + 2];
                                    let _ = file.write_all(&[r, g, b]);
                                }
                            }
                        }
                        eprintln!("Frame dumped to {:?}", path);
                    }
                    self.dumped = true;
                }
            }

            Ok(())
        }
    }
    #[cfg(feature = "kms_cpu_scanout")]
    impl PresentBackend for KmsCpuPresenter {
        fn preferred_format(&self) -> wgpu::TextureFormat { wgpu::TextureFormat::Bgra8Unorm }
        fn begin_frame(&mut self, device: &wgpu::Device) -> Result<FrameCtx> {
            let encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") });
            Ok(FrameCtx {
                encoder,
                view: self.target_view.clone(),
                inner: FrameInner::Kms,
            })
        }
        fn end_frame(
            &mut self,
            frame: FrameCtx,
            device: &wgpu::Device,
            queue: &wgpu::Queue,
        ) -> Result<()> {
            let mut encoder = frame.encoder;
            // Copy rendered image into readback buffer
            self.copy_to_readback(&mut encoder);
            queue.submit([encoder.finish()]);
            // Map, memcpy into dumb, and page flip
            self.blit_readback_to_dumb(device)?;
            self.card
                .page_flip(self.crtc, self.bufs[self.back].fb, PageFlipFlags::EVENT, None)
                .map_err(|e| annotate_eacces(e, "page_flip"))?;
            // Wait for vblank flip event on our CRTC
            loop {
                let evs = self
                    .card
                    .receive_events()
                    .map_err(|e| annotate_eacces(e, "receive_events"))?;
                let mut done = false;
                for ev in evs {
                    if let Event::PageFlip(e) = ev { if e.crtc == self.crtc { done = true; } }
                }
                if done { break; }
                std::thread::sleep(Duration::from_millis(1));
            }
            // swap back/front
            self.back ^= 1;
            Ok(())
        }
    }

    // ----- entry point combining both presenters -----
    pub async fn run() -> Result<()> {
        // 1) DRM pick
        let (card, pick) = open_card_and_pick().context("pick DRM card/connector/mode")?;

        // 2) Decide backends based on WGPU_BACKEND env var, then create instance.
        //    Default (no env var): try Vulkan first, fall back to GL.
        //    WGPU_BACKEND=vulkan  -> Vulkan only (fast path, may use lavapipe software renderer)
        //    WGPU_BACKEND=gl      -> GL only (Mesa v3d on Pi; skips DRM surface attempt)
        let env_backend = std::env::var("WGPU_BACKEND").ok();
        let allowed_backends = match env_backend.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some(s) if s.contains("gl") && !s.contains("vulkan") => {
                log::info!("WGPU_BACKEND=gl: using GL backend only");
                wgpu::Backends::GL
            }
            Some(s) if s.contains("vulkan") || s.contains("vk") => {
                log::info!("WGPU_BACKEND=vulkan: using Vulkan backend only");
                wgpu::Backends::VULKAN
            }
            Some(other) => {
                log::warn!("WGPU_BACKEND={other:?} unrecognised, defaulting to Vulkan+GL");
                wgpu::Backends::VULKAN | wgpu::Backends::GL
            }
            None => {
                log::info!("WGPU_BACKEND not set: will try Vulkan, fall back to GL");
                wgpu::Backends::VULKAN | wgpu::Backends::GL
            }
        };
        // Leaked so Surface<'static> can be stored in SurfacePresenter.
        let instance: &'static wgpu::Instance = Box::leak(Box::new(wgpu::Instance::new(
            &wgpu::InstanceDescriptor {
                backends: allowed_backends,
                flags: wgpu::InstanceFlags::empty(),
                ..Default::default()
            },
        )));

        // 3) Try Vulkan direct-to-DRM surface (requires VK_EXT_acquire_drm_display).
        //    This only makes sense when Vulkan is in the allowed set.
        let surface_opt: Option<wgpu::Surface<'static>> =
            if allowed_backends.contains(wgpu::Backends::VULKAN) {
                log::info!("attempting Vulkan DRM surface (VK_EXT_acquire_drm_display)...");
                let res = unsafe {
                    use wgpu::SurfaceTargetUnsafe;
                    instance.create_surface_unsafe(SurfaceTargetUnsafe::Drm {
                        fd: card.raw_fd(),
                        plane: 0,
                        connector_id: pick.connector_id,
                        width: pick.width,
                        height: pick.height,
                        refresh_rate: pick.refresh_millihz,
                    })
                };
                match res {
                    Ok(s) => {
                        log::info!("Vulkan DRM surface created successfully");
                        Some(s)
                    }
                    Err(e) => {
                        log::info!("Vulkan DRM surface unavailable ({e:#}); will use KMS CPU fallback");
                        None
                    }
                }
            } else {
                log::info!("Vulkan not in allowed backends — skipping DRM surface, using KMS CPU fallback");
                None
            };

        // 4) Pick adapter/device based on whether we have a surface
        let (_adapter, device, queue, mut backend): (wgpu::Adapter, wgpu::Device, wgpu::Queue, Box<dyn PresentBackend>) =
            match surface_opt {
                Some(surface) => {
                    let adapter = instance
                        .request_adapter(&wgpu::RequestAdapterOptions {
                            power_preference: wgpu::PowerPreference::HighPerformance,
                            compatible_surface: Some(&surface),
                            force_fallback_adapter: false,
                        })
                        .await
                        .context("No suitable Vulkan adapter for DRM surface")?;
                    {
                        let info = adapter.get_info();
                        log::info!("using adapter: name='{}' backend={:?} type={:?}", info.name, info.backend, info.device_type);
                    }
                    let (device, queue) = adapter
                        .request_device(&wgpu::DeviceDescriptor {
                            label: Some("device"),
                            required_features: wgpu::Features::empty(),
                            required_limits: wgpu::Limits::downlevel_defaults(),
                            ..Default::default()
                        })
                        .await?;
                    let presenter = SurfacePresenter::new(surface, &adapter, &device, pick.width, pick.height);
                    (adapter, device, queue, Box::new(presenter))
                }
                None => {
                    log::info!("using KMS CPU fallback: render offscreen, blit to dumb buffer, page-flip");
                    #[cfg(feature = "kms_cpu_scanout")]
                    {
                        let adapter = instance
                            .request_adapter(&wgpu::RequestAdapterOptions {
                                power_preference: wgpu::PowerPreference::HighPerformance,
                                compatible_surface: None,
                                force_fallback_adapter: false,
                            })
                            .await
                            .context("No suitable adapter for offscreen KMS scanout")?;
                        {
                            let info = adapter.get_info();
                            log::info!("using adapter: name='{}' backend={:?} type={:?}", info.name, info.backend, info.device_type);
                        }
                        let (device, queue) = adapter
                            .request_device(&wgpu::DeviceDescriptor {
                                label: Some("offscreen_device"),
                                required_features: wgpu::Features::empty(),
                                required_limits: wgpu::Limits::downlevel_defaults(),
                                ..Default::default()
                            })
                            .await?;
                        let presenter = KmsCpuPresenter::new(card.try_clone()?, pick, &device)
                            .context("init KMS CPU presenter")?;
                        (adapter, device, queue, Box::new(presenter))
                    }
                    #[cfg(not(feature = "kms_cpu_scanout"))]
                    {
                        return Err(anyhow!(
                            "wgpu DRM surface create failed and kms_cpu_scanout not enabled"
                        ));
                    }
                }
            };

        // 5) Single renderer (format comes from the presenter)
        let renderer = Renderer::new(&device, backend.preferred_format());

        // 6) Event loop
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

            let t = start.elapsed().as_secs_f32();
            renderer.update_time(&queue, t);

            let mut frame = backend.begin_frame(&device)?;
            renderer.record(&mut frame.encoder, &frame.view);
            backend.end_frame(frame, &device, &queue)?;
        }

        Ok(())
    }

    // If we hit EACCES (permission denied), annotate with actionable guidance for DRM master/seat.
    fn annotate_eacces(err: std::io::Error, where_: &str) -> anyhow::Error {
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
