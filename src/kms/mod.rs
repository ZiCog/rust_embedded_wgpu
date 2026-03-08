
use anyhow::{anyhow, Context, Result};
use std::{fs::File, io::ErrorKind, os::fd::{AsFd, AsRawFd, BorrowedFd}, time::Duration};
use drm::buffer::Buffer as _; // for .pitch()
use drm::control::{self as ctrl, Device as ControlDevice, Event, ModeTypeFlags, PageFlipFlags};

// ----- DRM card wrapper -----
struct Card(File);
impl drm::Device for Card {}
impl ctrl::Device for Card {}
impl AsFd for Card { fn as_fd(&self) -> BorrowedFd<'_> { self.0.as_fd() } }
impl Card {
    fn raw_fd(&self) -> i32 { self.0.as_raw_fd() }
    fn try_clone(&self) -> std::io::Result<Card> { Ok(Card(self.0.try_clone()?)) }
}

#[derive(Debug, Clone, Copy)]
pub struct DrmPick { pub connector_id: u32, pub width: u32, pub height: u32, pub refresh_millihz: u32 }

fn annotate_eacces(err: std::io::Error, where_: &str) -> anyhow::Error {
    if err.kind() == ErrorKind::PermissionDenied {
        anyhow!("{}: Permission denied. Run on a VT and/or ensure DRM master.", where_)
    } else { anyhow!(err).context(where_.to_string()) }
}

fn open_card_and_pick() -> Result<(Card, DrmPick)> {
    for i in 0..=9 {
        let path = format!("/dev/dri/card{}", i);
        let Ok(f) = File::options().read(true).write(true).open(&path) else { continue };
        let card = Card(f);
        let Ok(res) = card.resource_handles() else { continue };
        for &conn in res.connectors().iter() {
            let Ok(info) = card.get_connector(conn, false) else { continue };
            if info.state() != ctrl::connector::State::Connected { continue }
            let modes = info.modes();
            if modes.is_empty() { continue }
            let m = modes.iter().find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).unwrap_or(&modes[0]);
            return Ok((card, DrmPick { connector_id: conn.into(), width: m.size().0 as u32, height: m.size().1 as u32, refresh_millihz: (m.vrefresh() as u32)*1000 }));
        }
    }
    anyhow::bail!("No DRM card with a connected connector/mode found")
}

pub struct DbBuf { dbuf: ctrl::dumbbuffer::DumbBuffer, fb: ctrl::framebuffer::Handle }

pub struct KmsCpuPresenter {
    card: Card,
    crtc: ctrl::crtc::Handle,
    bufs: [DbBuf; 2],
    pub width: u32,
    pub height: u32,
    back: usize,
    dumb_pitch: u32,
    target: wgpu::Texture,
    pub target_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    copy_bpr: u32,
}

impl KmsCpuPresenter {
    fn new(card: Card, pick: DrmPick, device: &wgpu::Device) -> Result<Self> {
        let res = card.resource_handles().context("resource_handles")?;
        let (conn, conn_info) = res
            .connectors()
            .iter()
            .filter_map(|&c| card.get_connector(c, true).ok().map(|i| (c, i)))
            .find(|(_, i)| i.state() == ctrl::connector::State::Connected && !i.modes().is_empty())
            .ok_or_else(|| anyhow!("no connected connector found"))?;
        let enc = *conn_info.encoders().get(0).ok_or_else(|| anyhow!("no encoder for connector"))?;
        let enc_info = card.get_encoder(enc).context("get_encoder")?;
        let crtc = enc_info.crtc().ok_or_else(|| anyhow!("no crtc available"))?;
        let mode = *conn_info.modes().get(0).ok_or_else(|| anyhow!("no modes"))?;
        let width = pick.width.max(1);
        let height = pick.height.max(1);
        let make_buf = |label: &str| -> Result<DbBuf> {
            let mut dbuf = card
                .create_dumb_buffer((width, height), drm::buffer::DrmFourcc::Xrgb8888, 32)
                .map_err(|e| annotate_eacces(e, label))?;
            {
                let mut map = card.map_dumb_buffer(&mut dbuf).map_err(|e| annotate_eacces(e, "map_dumb_buffer"))?;
                for px in map.chunks_exact_mut(4) { px[0]=0x10; px[1]=0x10; px[2]=0x10; px[3]=0xFF; }
            }
            let fb = card.add_framebuffer(&dbuf, 24, 32).map_err(|e| annotate_eacces(e, "add_framebuffer"))?;
            Ok(DbBuf { dbuf, fb })
        };
        let b0 = make_buf("create_dumb_buffer[0]")?;
        let b1 = make_buf("create_dumb_buffer[1]")?;
        card.set_crtc(crtc, Some(b0.fb), (0,0), &[conn], Some(mode)).map_err(|e| annotate_eacces(e, "set_crtc"))?;
        let dumb_pitch = b0.dbuf.pitch() as u32;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen_target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1},
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let unpadded_bpr = width * 4;
        let mut copy_bpr = ((unpadded_bpr + 255) / 256) * 256;
        if dumb_pitch % 256 == 0 { copy_bpr = dumb_pitch; }
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: Some("readback"), size: (copy_bpr as u64) * (height as u64), usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        Ok(Self { card, crtc, bufs:[b0,b1], width, height, back:1, dumb_pitch, target, target_view, readback, copy_bpr })
    }
    pub fn preferred_format(&self) -> wgpu::TextureFormat { wgpu::TextureFormat::Bgra8Unorm }
    fn copy_to_readback(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &self.readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.copy_bpr), rows_per_image: Some(self.height) } },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
    }
    fn blit_readback_to_dumb(&mut self, device: &wgpu::Device) -> Result<()> {
        let _ = device.poll(wgpu::PollType::Wait);
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait);
        let data = slice.get_mapped_range();
        let h = self.height as usize; let pitch = self.dumb_pitch as usize; let copy_bpr = self.copy_bpr as usize;
        let mut map = self.card.map_dumb_buffer(&mut self.bufs[self.back].dbuf).map_err(|e| annotate_eacces(e, "map_dumb_buffer"))?;
        if copy_bpr == pitch { let bytes = copy_bpr * h; map[..bytes].copy_from_slice(&data[..bytes]); }
        else { for y in 0..h { let src = &data[y*copy_bpr..][..(self.width as usize * 4)]; let row = &mut map[y*pitch..][..(self.width as usize * 4)]; row.copy_from_slice(src); } }
        drop(map); drop(data); self.readback.unmap(); Ok(())
    }
    pub fn begin_frame(&self, device: &wgpu::Device) -> wgpu::CommandEncoder { device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("encoder") }) }
    pub fn end_frame(&mut self, mut encoder: wgpu::CommandEncoder, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<()> {
        self.copy_to_readback(&mut encoder); queue.submit([encoder.finish()]); self.blit_readback_to_dumb(device)?;
        self.card.page_flip(self.crtc, self.bufs[self.back].fb, PageFlipFlags::EVENT, None).map_err(|e| annotate_eacces(e, "page_flip"))?;
        loop { let evs = self.card.receive_events().map_err(|e| annotate_eacces(e, "receive_events"))?; let mut done=false; for ev in evs { if let Event::PageFlip(e)=ev { if e.crtc==self.crtc { done=true; } } } if done { break } std::thread::sleep(Duration::from_millis(1)); }
        self.back ^= 1; Ok(())
    }
}

pub struct KmsContext { pub device: wgpu::Device, pub queue: wgpu::Queue, pub presenter: KmsCpuPresenter }

pub fn init() -> Result<KmsContext> {
    let _ = env_logger::try_init();
    let (card, pick) = open_card_and_pick().context("pick DRM card/connector/mode")?;
    let instance: &'static wgpu::Instance = Box::leak(Box::new(wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN | wgpu::Backends::GL, ..Default::default() })));
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: None, force_fallback_adapter: false }))
        .context("No suitable adapter for offscreen KMS scanout")?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("offscreen_device"), required_features: wgpu::Features::empty(), required_limits: wgpu::Limits::downlevel_defaults(), ..Default::default() }))?;
    let presenter = KmsCpuPresenter::new(card, pick, &device).context("init KMS CPU presenter")?;
    Ok(KmsContext { device, queue, presenter })
}

pub fn frame_loop(mut ctx: KmsContext, mut render: impl FnMut(&wgpu::Device, &wgpu::Queue, &mut wgpu::CommandEncoder, &wgpu::TextureView) -> Result<()>) -> Result<()> {
    let mut last = std::time::Instant::now();
    loop {
        let now = std::time::Instant::now();
        if now.duration_since(last) < Duration::from_millis(16) { std::thread::sleep(Duration::from_millis(1)); continue }
        last = now;
        let mut encoder = ctx.presenter.begin_frame(&ctx.device);
        render(&ctx.device, &ctx.queue, &mut encoder, &ctx.presenter.target_view)?;
        ctx.presenter.end_frame(encoder, &ctx.device, &ctx.queue)?;
    }
}
