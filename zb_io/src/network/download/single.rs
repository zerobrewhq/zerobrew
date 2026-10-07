use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::CONTENT_LENGTH;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::warn;

use crate::network::tls::shared_tls_config;
use crate::progress::InstallProgress;
use crate::storage::blob::BlobCache;
use zb_core::Error;

use super::auth::{TokenCache, TokenEndpoint, fetch_download_response_internal, prefetch_tokens};
use super::{
    DownloadError, DownloadProgressCallback, GLOBAL_DOWNLOAD_CONCURRENCY, MAX_DOWNLOAD_ATTEMPTS,
};

fn get_alternate_urls(primary_url: &str) -> Vec<String> {
    let mut alternates = Vec::new();

    if let Ok(mirrors) = std::env::var("HOMEBREW_BOTTLE_MIRRORS") {
        for mirror in mirrors.split(',') {
            let mirror = mirror.trim();
            if !mirror.is_empty()
                && let Some(alt) = transform_url_to_mirror(primary_url, mirror)
            {
                alternates.push(alt);
            }
        }
    }

    alternates
}

fn transform_url_to_mirror(url: &str, mirror_domain: &str) -> Option<String> {
    if url.contains("ghcr.io") {
        Some(url.replace("ghcr.io", mirror_domain))
    } else {
        None
    }
}

/// Downloads bottles into the blob cache over one connection pool. Every
/// request reuses the registry and CDN connections of the ones before it,
/// and registry tokens are fetched up front for a whole plan where the
/// registry is known; see [`Downloader::prefetch_tokens`].
pub struct Downloader {
    client: reqwest::Client,
    pub(crate) blob_cache: BlobCache,
    token_cache: TokenCache,
    token_endpoints: HashMap<String, TokenEndpoint>,
}

impl Downloader {
    pub fn new(blob_cache: BlobCache) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("zerobrew/0.1")
            .use_preconfigured_tls((*shared_tls_config()).clone())
            .pool_max_idle_per_host(GLOBAL_DOWNLOAD_CONCURRENCY)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .http2_adaptive_window(true)
            .http2_initial_stream_window_size(Some(2 * 1024 * 1024))
            .http2_initial_connection_window_size(Some(4 * 1024 * 1024))
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            blob_cache,
            token_cache: Arc::new(RwLock::new(HashMap::new())),
            token_endpoints: TokenEndpoint::known(),
        }
    }

    /// Treat `host` as a registry handing out tokens at `endpoint`.
    #[cfg(test)]
    pub(crate) fn with_token_endpoint(mut self, host: &str, endpoint: TokenEndpoint) -> Self {
        self.token_endpoints.insert(host.to_string(), endpoint);
        self
    }

    pub fn remove_blob(&self, sha256: &str) -> bool {
        self.blob_cache.remove_blob(sha256).unwrap_or(false)
    }

    /// Fetch one registry token covering every blob in `urls` before they
    /// are downloaded, so none of them pays the 401 round trip.
    pub(crate) async fn prefetch_tokens(&self, urls: &[String]) {
        prefetch_tokens(&self.client, &self.token_cache, &self.token_endpoints, urls).await;
    }

    pub async fn download(&self, url: &str, expected_sha256: &str) -> Result<PathBuf, Error> {
        self.download_with_progress(url, expected_sha256, None, None)
            .await
    }

    pub async fn download_with_progress(
        &self,
        url: &str,
        expected_sha256: &str,
        name: Option<String>,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<PathBuf, Error> {
        if self.blob_cache.has_blob(expected_sha256) {
            if let (Some(cb), Some(n)) = (&progress, &name) {
                cb(InstallProgress::DownloadCompleted {
                    name: n.clone(),
                    total_bytes: 0,
                });
            }
            return Ok(self.blob_cache.blob_path(expected_sha256));
        }

        let mut urls = vec![url.to_string()];
        urls.extend(get_alternate_urls(url));
        self.download_from(&urls, expected_sha256, name, progress)
            .await
    }

    /// Download from the first of `urls` that works. Each URL gets up to
    /// [`MAX_DOWNLOAD_ATTEMPTS`] tries with backoff while its failures look
    /// transient; a final failure moves on to the next URL.
    async fn download_from(
        &self,
        urls: &[String],
        expected_sha256: &str,
        name: Option<String>,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<PathBuf, Error> {
        let mut last_error = None;

        for url in urls {
            for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
                match self
                    .download_once(url, expected_sha256, name.clone(), progress.clone())
                    .await
                {
                    Ok(path) => return Ok(path),
                    Err(DownloadError { error, transient }) => {
                        let retry = transient && attempt < MAX_DOWNLOAD_ATTEMPTS;
                        warn!(
                            url,
                            attempt,
                            error = %error,
                            retry,
                            "download attempt failed"
                        );
                        last_error = Some(error);
                        if !retry {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250 << (attempt - 1))).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| Error::NetworkFailure {
            message: "all download attempts failed".to_string(),
        }))
    }

    async fn download_once(
        &self,
        url: &str,
        expected_sha256: &str,
        name: Option<String>,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<PathBuf, DownloadError> {
        let response =
            fetch_download_response_internal(&self.client, &self.token_cache, url).await?;
        download_response_internal(&self.blob_cache, response, expected_sha256, name, progress)
            .await
    }
}

/// Stream a response into the blob cache, verifying its checksum.
async fn download_response_internal(
    blob_cache: &BlobCache,
    response: reqwest::Response,
    expected_sha256: &str,
    name: Option<String>,
    progress: Option<DownloadProgressCallback>,
) -> Result<PathBuf, DownloadError> {
    let total_bytes = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    if let (Some(cb), Some(n)) = (&progress, &name) {
        cb(InstallProgress::DownloadStarted {
            name: n.clone(),
            total_bytes,
        });
    }

    let mut writer = blob_cache
        .start_write(expected_sha256)
        .map_err(|e| DownloadError::permanent(Error::network("failed to create blob writer")(e)))?;

    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;

    while let Some(chunk) = stream.next().await {
        // A connection dropped mid-body; the next attempt starts over.
        let chunk = chunk
            .map_err(|e| DownloadError::transient(Error::network("failed to read chunk")(e)))?;

        downloaded += chunk.len() as u64;
        hasher.update(&chunk);
        writer
            .write_all(&chunk)
            .map_err(|e| DownloadError::permanent(Error::network("failed to write chunk")(e)))?;

        if let (Some(cb), Some(n)) = (&progress, &name) {
            cb(InstallProgress::DownloadProgress {
                name: n.clone(),
                downloaded,
                total_bytes,
            });
        }
    }

    let actual_hash = crate::checksum::sha256_hex(hasher);

    if actual_hash != expected_sha256 {
        // Most often a truncated or mangled transfer; the next attempt may
        // get the real thing.
        return Err(DownloadError::transient(Error::ChecksumMismatch {
            expected: expected_sha256.to_string(),
            actual: actual_hash,
        }));
    }

    writer
        .flush()
        .map_err(|e| DownloadError::permanent(Error::network("failed to flush download")(e)))?;

    if let (Some(cb), Some(n)) = (&progress, &name) {
        cb(InstallProgress::DownloadCompleted {
            name: n.clone(),
            total_bytes: downloaded,
        });
    }

    writer.commit().map_err(DownloadError::permanent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use wiremock::matchers::{header_exists, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const HELLO_SHA: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    fn downloader(tmp: &TempDir) -> Downloader {
        Downloader::new(BlobCache::new(tmp.path()).unwrap())
    }

    #[tokio::test]
    async fn valid_checksum_passes() {
        let mock_server = MockServer::start().await;
        let content = b"hello world";

        Mock::given(method("GET"))
            .and(path("/test.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);

        let url = format!("{}/test.tar.gz", mock_server.uri());
        let result = downloader.download(&url, HELLO_SHA).await;

        assert!(result.is_ok());
        let blob_path = result.unwrap();
        assert!(blob_path.exists());
        assert_eq!(std::fs::read(&blob_path).unwrap(), content);
    }

    #[tokio::test]
    async fn mismatch_deletes_blob_and_errors() {
        let mock_server = MockServer::start().await;
        let content = b"hello world";
        let wrong_sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

        Mock::given(method("GET"))
            .and(path("/test.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .expect(MAX_DOWNLOAD_ATTEMPTS as u64)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);

        let url = format!("{}/test.tar.gz", mock_server.uri());
        let result = downloader.download(&url, wrong_sha256).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }));

        let blob_path = tmp
            .path()
            .join("blobs")
            .join(format!("{wrong_sha256}.tar.gz"));
        assert!(!blob_path.exists());

        let tmp_path = tmp
            .path()
            .join("tmp")
            .join(format!("{wrong_sha256}.tar.gz.part"));
        assert!(!tmp_path.exists());
    }

    #[tokio::test]
    async fn skips_download_if_blob_exists() {
        let mock_server = MockServer::start().await;
        let content = b"hello world";

        Mock::given(method("GET"))
            .and(path("/test.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .expect(0)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();

        let mut writer = blob_cache.start_write(HELLO_SHA).unwrap();
        writer.write_all(content).unwrap();
        writer.commit().unwrap();

        let downloader = Downloader::new(blob_cache);
        let url = format!("{}/test.tar.gz", mock_server.uri());
        let result = downloader.download(&url, HELLO_SHA).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn retries_a_server_error_and_then_succeeds() {
        let mock_server = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();

        Mock::given(method("GET"))
            .and(path("/flaky.tar.gz"))
            .respond_with(move |_: &Request| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                } else {
                    ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec())
                }
            })
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/flaky.tar.gz", mock_server.uri());
        let path = downloader(&tmp).download(&url, HELLO_SHA).await.unwrap();

        assert!(path.exists());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_client_error_is_not_retried() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/missing.tar.gz"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/missing.tar.gz", mock_server.uri());
        let err = downloader(&tmp)
            .download(&url, HELLO_SHA)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("HTTP 404"), "{err}");
    }

    #[tokio::test]
    async fn falls_back_to_the_next_url_after_the_first_gives_up() {
        let primary = MockServer::start().await;
        let mirror = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/x.tar.gz"))
            .respond_with(ResponseTemplate::new(500))
            .expect(MAX_DOWNLOAD_ATTEMPTS as u64)
            .mount(&primary)
            .await;
        Mock::given(method("GET"))
            .and(path("/x.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec()))
            .expect(1)
            .mount(&mirror)
            .await;

        let tmp = TempDir::new().unwrap();
        let urls = [
            format!("{}/x.tar.gz", primary.uri()),
            format!("{}/x.tar.gz", mirror.uri()),
        ];
        let path = downloader(&tmp)
            .download_from(&urls, HELLO_SHA, None, None)
            .await
            .unwrap();

        assert_eq!(std::fs::read(path).unwrap(), b"hello world");
    }

    /// A registry that demands a token: anonymous requests get a challenge.
    async fn registry(server: &MockServer, blob_path: &str) {
        Mock::given(method("GET"))
            .and(path(blob_path))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello world".to_vec()))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(blob_path))
            .respond_with(ResponseTemplate::new(401).append_header(
                "WWW-Authenticate",
                format!(
                    "Bearer realm=\"{}/token\",service=\"test\",scope=\"repository:homebrew/core/x:pull\"",
                    server.uri()
                ),
            ))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn answers_a_401_challenge_with_a_token() {
        let server = MockServer::start().await;
        registry(&server, "/v2/homebrew/core/x/blobs/sha256:abc").await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("service", "test"))
            .and(query_param("scope", "repository:homebrew/core/x:pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "test-token-12345"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);
        let url = format!("{}/v2/homebrew/core/x/blobs/sha256:abc", server.uri());

        let path = downloader.download(&url, HELLO_SHA).await.unwrap();
        assert!(path.exists());

        // The token is cached for the scope: a second blob of the same
        // formula needs no challenge.
        downloader.remove_blob(HELLO_SHA);
        downloader.download(&url, HELLO_SHA).await.unwrap();
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path().contains("/blobs/"))
                .count(),
            3,
            "challenge, authenticated GET, then one authenticated GET"
        );
    }

    #[tokio::test]
    async fn prefetches_one_token_for_every_blob_of_a_known_registry() {
        let server = MockServer::start().await;
        for name in ["a", "b", "c"] {
            registry(
                &server,
                &format!("/v2/homebrew/core/{name}/blobs/sha256:abc"),
            )
            .await;
        }
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("scope", "repository:homebrew/core/a:pull"))
            .and(query_param("scope", "repository:homebrew/core/b:pull"))
            .and(query_param("scope", "repository:homebrew/core/c:pull"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "shared-token",
                "expires_in": 300
            })))
            .expect(1)
            .mount(&server)
            .await;

        let host = server.uri().trim_start_matches("http://").to_string();
        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp).with_token_endpoint(
            &host,
            TokenEndpoint {
                realm: format!("{}/token", server.uri()),
                service: "test".into(),
            },
        );
        let urls: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| format!("{}/v2/homebrew/core/{name}/blobs/sha256:abc", server.uri()))
            .collect();

        downloader.prefetch_tokens(&urls).await;
        for url in &urls {
            downloader.download(url, HELLO_SHA).await.unwrap();
            downloader.remove_blob(HELLO_SHA);
        }

        let challenged = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| {
                r.url.path().contains("/blobs/") && !r.headers.contains_key("authorization")
            })
            .count();
        assert_eq!(
            challenged, 0,
            "every blob GET carried the pre-fetched token"
        );
    }

    #[tokio::test]
    async fn prefetch_skips_unknown_registries_and_cached_scopes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let downloader = downloader(&tmp);
        downloader
            .prefetch_tokens(&[format!(
                "{}/v2/homebrew/core/a/blobs/sha256:abc",
                server.uri()
            )])
            .await;
    }
}
