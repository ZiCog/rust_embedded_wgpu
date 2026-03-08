// Example: run upstream hello_triangle using the headless KMS library
// cargo run --release --features wgpu_drm,kms_cpu_scanout,examples_upstream --example hello_triangle

use anyhow::Result;
use rust_embedded_wgpu::kms::{self, frame_loop};

#[path = "upstream/triangle_upstream.rs"]
mod triangle_upstream;

fn main() -> Result<()> {
    let mut ctx = kms::init()?;
    let mut app = triangle_upstream::HelloTriangle::new(
        &ctx.device,
        &ctx.queue,
        ctx.presenter.preferred_format(),
        triangle_upstream::ExampleSize { width: ctx.presenter.width, height: ctx.presenter.height },
    )?;
    frame_loop(ctx, move |encoder, view| app.render(encoder, view))
}
