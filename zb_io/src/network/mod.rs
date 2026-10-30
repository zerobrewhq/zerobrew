pub mod api;
pub mod cache;
pub mod download;
pub mod suggest;
pub mod tap_formula;
#[cfg(not(target_os = "android"))]
pub(crate) mod tls;

pub use api::ApiClient;
pub use cache::{ApiCache, CacheEntry};
pub use download::{
    DownloadProgressCallback, DownloadRequest, DownloadResult, Downloader, ParallelDownloader,
};
