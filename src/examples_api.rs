//! Minimal adapter API for running upstream wgpu examples on our headless KMS path.
//!
//! Upstream examples (hello_triangle, cube) typically create a winit window and a
//! wgpu Surface, then render into the swapchain's TextureView each frame.
//!
//! To keep their logic nearly intact, we ask the adapted example to:
//! - construct itself with an existing Device/Queue, output format, and size
//! - render into a provided &TextureView using a provided CommandEncoder
//!
//! Our KMS presenter owns the offscreen render target and calls `render` each
//! frame between `begin_frame` and `end_frame`.

/// Logical output size for the example.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExampleSize {
    pub width: u32,
    pub height: u32,
}

impl ExampleSize {
    pub fn new(width: u32, height: u32) -> Self { Self { width, height } }
}

/// The minimal interface an adapted upstream example must implement.
///
/// Guidance for adapting upstream sources:
/// - Move pipeline/buffer/shader setup into `new`.
/// - Replace any surface/swapchain usage with the `format`/`size` provided here.
/// - In `render`, record all passes into the given `encoder` targeting `view`.
pub trait ExampleApp {
    /// Create the example using an existing device/queue and the target output
    /// description (format + size). Implementations may cache the `format`/`size`
    /// to rebuild pipelines if dimensions change later.
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        size: ExampleSize,
    ) -> anyhow::Result<Self>
    where
        Self: Sized;

    /// Record commands that render a frame into `view` using `encoder`.
    fn render(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
    ) -> anyhow::Result<()>;
}
