#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub(crate) mod macho;
mod phar;
pub(crate) mod text;

#[cfg(target_os = "linux")]
pub use linux::patch_placeholders;

#[cfg(target_os = "macos")]
pub use macos::{codesign_and_strip_xattrs, patch_homebrew_placeholders};
