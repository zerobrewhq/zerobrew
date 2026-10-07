mod auth;
mod parallel;
mod single;

use std::path::PathBuf;
use std::sync::Arc;

use zb_core::Error;

use crate::progress::InstallProgress;

pub type DownloadProgressCallback = Arc<dyn Fn(InstallProgress) + Send + Sync>;

/// Global download concurrency limit
/// Total number of concurrent connections across all downloads to avoid
/// overwhelming servers and the local network. Based on industry best practices
/// (npm uses 20-50, we use a conservative 20 for HTTP/1.1 compatibility).
const GLOBAL_DOWNLOAD_CONCURRENCY: usize = 20;

/// How many times one URL is tried before moving on to the next mirror.
const MAX_DOWNLOAD_ATTEMPTS: u32 = 3;

/// A download failure and whether trying the same URL again could help.
#[derive(Debug)]
pub(crate) struct DownloadError {
    pub(crate) error: Error,
    /// Connection trouble, a server error or a bad body: worth a retry. A
    /// client error such as a 404 is not.
    pub(crate) transient: bool,
}

impl DownloadError {
    pub(crate) fn transient(error: Error) -> Self {
        Self {
            error,
            transient: true,
        }
    }

    pub(crate) fn permanent(error: Error) -> Self {
        Self {
            error,
            transient: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub name: String,
    pub sha256: String,
    pub blob_path: PathBuf,
    pub index: usize,
}

pub use parallel::{DownloadRequest, ParallelDownloader};
pub use single::Downloader;
