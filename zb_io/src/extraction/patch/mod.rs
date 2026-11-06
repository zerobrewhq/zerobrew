#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub(crate) mod macho;
mod phar;
pub(crate) mod text;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub use linux::patch_placeholders;

#[cfg(target_os = "macos")]
pub use macos::relocate_keg;
