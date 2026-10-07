use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::storage::blob::BlobCache;
use zb_core::Error;

use super::single::Downloader;
use super::{DownloadProgressCallback, DownloadResult, GLOBAL_DOWNLOAD_CONCURRENCY};

pub struct DownloadRequest {
    pub url: String,
    pub sha256: String,
    pub name: String,
}

type InflightMap = HashMap<String, Arc<tokio::sync::broadcast::Sender<Result<PathBuf, String>>>>;

#[derive(Clone)]
pub struct ParallelDownloader {
    downloader: Arc<Downloader>,
    semaphore: Arc<Semaphore>,
    inflight: Arc<Mutex<InflightMap>>,
}

impl ParallelDownloader {
    pub fn new(blob_cache: BlobCache) -> Self {
        Self::with_concurrency(blob_cache, GLOBAL_DOWNLOAD_CONCURRENCY)
    }

    pub fn with_concurrency(blob_cache: BlobCache, concurrency: usize) -> Self {
        Self {
            downloader: Arc::new(Downloader::new(blob_cache)),
            semaphore: Arc::new(Semaphore::new(concurrency)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The URLs in `requests` whose blobs are not cached yet.
    fn missing_urls(&self, requests: &[DownloadRequest]) -> Vec<String> {
        requests
            .iter()
            .filter(|req| !self.downloader.blob_cache.has_blob(&req.sha256))
            .map(|req| req.url.clone())
            .collect()
    }

    pub fn remove_blob(&self, sha256: &str) -> bool {
        self.downloader.remove_blob(sha256)
    }

    pub async fn download_single(
        &self,
        request: DownloadRequest,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<PathBuf, Error> {
        Self::download_with_dedup(
            self.downloader.clone(),
            self.semaphore.clone(),
            self.inflight.clone(),
            request,
            progress,
        )
        .await
    }

    pub async fn download_all(
        &self,
        requests: Vec<DownloadRequest>,
    ) -> Result<Vec<PathBuf>, Error> {
        self.download_all_with_progress(requests, None).await
    }

    pub async fn download_all_with_progress(
        &self,
        requests: Vec<DownloadRequest>,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<Vec<PathBuf>, Error> {
        self.downloader
            .prefetch_tokens(&self.missing_urls(&requests))
            .await;
        let handles: Vec<_> = requests
            .into_iter()
            .map(|req| {
                let downloader = self.downloader.clone();
                let semaphore = self.semaphore.clone();
                let inflight = self.inflight.clone();
                let progress = progress.clone();

                tokio::spawn(async move {
                    Self::download_with_dedup(downloader, semaphore, inflight, req, progress).await
                })
            })
            .collect();

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            let result = handle.await.map_err(Error::network("task join error"))??;
            results.push(result);
        }

        Ok(results)
    }

    pub fn download_streaming(
        &self,
        requests: Vec<DownloadRequest>,
        progress: Option<DownloadProgressCallback>,
    ) -> mpsc::Receiver<Result<DownloadResult, Error>> {
        let (tx, rx) = mpsc::channel(requests.len().max(1));
        let missing = self.missing_urls(&requests);
        let downloader = self.downloader.clone();
        let semaphore = self.semaphore.clone();
        let inflight = self.inflight.clone();

        tokio::spawn(async move {
            // One token for the whole plan before the first bottle request.
            downloader.prefetch_tokens(&missing).await;

            for (index, req) in requests.into_iter().enumerate() {
                let downloader = downloader.clone();
                let semaphore = semaphore.clone();
                let inflight = inflight.clone();
                let progress = progress.clone();
                let tx = tx.clone();
                let name = req.name.clone();
                let sha256 = req.sha256.clone();

                tokio::spawn(async move {
                    let result =
                        Self::download_with_dedup(downloader, semaphore, inflight, req, progress)
                            .await;
                    let _ = tx
                        .send(result.map(|blob_path| DownloadResult {
                            name,
                            sha256,
                            blob_path,
                            index,
                        }))
                        .await;
                });
            }
        });

        rx
    }

    async fn download_with_dedup(
        downloader: Arc<Downloader>,
        semaphore: Arc<Semaphore>,
        inflight: Arc<Mutex<InflightMap>>,
        req: DownloadRequest,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<PathBuf, Error> {
        let mut receiver = {
            let mut map = inflight.lock().await;

            if let Some(sender) = map.get(&req.sha256) {
                Some(sender.subscribe())
            } else {
                let (tx, _) = tokio::sync::broadcast::channel(1);
                map.insert(req.sha256.clone(), Arc::new(tx));
                None
            }
        };

        if let Some(ref mut rx) = receiver {
            let result = rx
                .recv()
                .await
                .map_err(Error::network("broadcast recv error"))?;

            return result.map_err(|msg| Error::NetworkFailure { message: msg });
        }

        let _permit = semaphore
            .acquire()
            .await
            .map_err(Error::network("semaphore error"))?;

        let result = downloader
            .download_with_progress(&req.url, &req.sha256, Some(req.name), progress)
            .await;

        {
            let mut map = inflight.lock().await;
            if let Some(sender) = map.remove(&req.sha256) {
                let broadcast_result = match &result {
                    Ok(path) => Ok(path.clone()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = sender.send(broadcast_result);
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::storage::blob::BlobCache;

    use super::super::GLOBAL_DOWNLOAD_CONCURRENCY;
    use super::*;

    #[tokio::test]
    async fn peak_concurrent_downloads_within_limit() {
        let mock_server = MockServer::start().await;
        let concurrent_count = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let content = b"test content";
        let count_clone = concurrent_count.clone();
        let max_clone = max_concurrent.clone();

        Mock::given(method("GET"))
            .respond_with(move |_: &wiremock::Request| {
                let current = count_clone.fetch_add(1, Ordering::SeqCst) + 1;
                max_clone.fetch_max(current, Ordering::SeqCst);

                std::thread::sleep(Duration::from_millis(50));

                count_clone.fetch_sub(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_bytes(content.to_vec())
            })
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let downloader = ParallelDownloader::new(blob_cache);

        let requests: Vec<_> = (0..5)
            .map(|i| {
                let sha256 = format!("{:064x}", i);
                DownloadRequest {
                    url: format!("{}/file{i}.tar.gz", mock_server.uri()),
                    sha256,
                    name: format!("pkg{i}"),
                }
            })
            .collect();

        let _ = downloader.download_all(requests).await;

        let peak = max_concurrent.load(Ordering::SeqCst);
        assert!(
            peak <= GLOBAL_DOWNLOAD_CONCURRENCY,
            "peak concurrent downloads was {peak}, expected <= {GLOBAL_DOWNLOAD_CONCURRENCY}"
        );
    }

    #[tokio::test]
    async fn same_blob_requested_multiple_times_fetches_once() {
        let mock_server = MockServer::start().await;
        let content = b"deduplicated content";

        let actual_sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(content);
            crate::checksum::sha256_hex(hasher)
        };

        Mock::given(method("GET"))
            .and(path("/dedup.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(content.to_vec())
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let downloader = ParallelDownloader::new(blob_cache);

        let requests: Vec<_> = (0..5)
            .map(|i| DownloadRequest {
                url: format!("{}/dedup.tar.gz", mock_server.uri()),
                sha256: actual_sha256.clone(),
                name: format!("dedup{i}"),
            })
            .collect();

        let results = downloader.download_all(requests).await.unwrap();

        assert_eq!(results.len(), 5);
        for path in &results {
            assert!(path.exists());
        }
    }
}
