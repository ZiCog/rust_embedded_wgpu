// KMS headless path only exists on Linux; avoid compiling it on macOS/Windows
#[cfg(target_os = "linux")]
pub mod kms;
