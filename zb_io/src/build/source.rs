use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::fs;
use zb_core::Error;

use crate::checksum::verify_sha256_bytes;
use crate::extraction::{extract_tarball, is_archive};

pub async fn download_and_extract_source(
    formula_name: &str,
    url: &str,
    expected_checksum: Option<&str>,
    work_dir: &Path,
    nounzip: bool,
) -> Result<PathBuf, Error> {
    let tarball_path = work_dir.join("source.tar.gz");
    download_source(url, &tarball_path).await?;

    verify_checksum(&tarball_path, expected_checksum, url).await?;

    let src_dir = work_dir.join("src");
    fs::create_dir_all(&src_dir)
        .await
        .map_err(Error::file("failed to create source directory"))?;

    if nounzip {
        // `using: :nounzip`: the download is a raw file (e.g. a prebuilt
        // binary). Stage it under its URL basename without unpacking, the
        // same layout Homebrew's UnpackStrategy::Uncompressed produces.
        // Permissions are left untouched; the formula's install handles them.
        let file_name = source_file_name(url, formula_name);
        fs::rename(&tarball_path, src_dir.join(&file_name))
            .await
            .map_err(Error::file("failed to stage raw source file"))?;
        return Ok(src_dir);
    }

    if !is_archive(&tarball_path)? {
        return Err(Error::UnsupportedFormula {
            name: formula_name.to_string(),
            reason: "unsupported archive format; zerobrew unpacks gzip, xz, \
                     zstd and zip"
                .to_string(),
        });
    }

    extract_tarball(&tarball_path, &src_dir)?;

    find_source_root(&src_dir).await
}

/// Basename of the requested URL for staging a raw (`nounzip`) download.
/// Falls back to the formula name when the URL path has no usable last
/// segment; the result never contains path separators or `..`.
fn source_file_name(url: &str, formula_name: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    // Drop the scheme and authority so a path-less URL yields no segment.
    let after_scheme = without_query
        .split_once("://")
        .map_or(without_query, |(_, rest)| rest);
    let path = after_scheme.split_once('/').map_or("", |(_, p)| p);
    let segment = path.rsplit('/').next().unwrap_or("");
    let candidate = Path::new(segment).file_name().and_then(|n| n.to_str());
    match candidate {
        Some(name) if !name.is_empty() && name != "." && name != ".." => name.to_string(),
        _ => Path::new(formula_name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("source")
            .to_string(),
    }
}

// Fail a source download only when the connection cannot be established or
// no bytes arrive for this long; the total transfer time is not capped.
const SOURCE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SOURCE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

async fn download_source(url: &str, dest: &Path) -> Result<(), Error> {
    download_source_with(url, dest, SOURCE_CONNECT_TIMEOUT, SOURCE_IDLE_TIMEOUT).await
}

async fn download_source_with(
    url: &str,
    dest: &Path,
    connect_timeout: Duration,
    idle_timeout: Duration,
) -> Result<(), Error> {
    let client = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .read_timeout(idle_timeout)
        .use_preconfigured_tls((*crate::network::tls::shared_tls_config()).clone())
        .build()
        .map_err(Error::network("failed to create HTTP client"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(Error::network("failed to download source"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::NetworkFailure {
            message: format!("source download returned HTTP {status}"),
        });
    }

    let bytes = response.bytes().await.map_err(|e| {
        if e.is_timeout() {
            Error::NetworkFailure {
                message: format!(
                    "source download from {url} timed out after {}s without receiving data",
                    idle_timeout.as_secs()
                ),
            }
        } else {
            Error::network("failed to read source response")(e)
        }
    })?;

    fs::write(dest, &bytes)
        .await
        .map_err(Error::file("failed to write source tarball"))
}

async fn verify_checksum(path: &Path, expected: Option<&str>, url: &str) -> Result<(), Error> {
    let bytes = fs::read(path)
        .await
        .map_err(Error::file("failed to read tarball for checksum"))?;

    verify_sha256_bytes(&bytes, expected).map_err(|e| match e {
        Error::ChecksumMismatch { .. } => e,
        Error::InvalidArgument { message } => Error::InvalidArgument {
            message: format!("invalid source checksum for '{url}': {message}"),
        },
        other => other,
    })
}

async fn find_source_root(src_dir: &Path) -> Result<PathBuf, Error> {
    let mut entries = fs::read_dir(src_dir)
        .await
        .map_err(Error::file("failed to read source directory"))?;

    let mut subdirs = Vec::new();
    let mut has_files = false;

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(Error::file("failed to read directory entry"))?
    {
        let ft = entry
            .file_type()
            .await
            .map_err(Error::file("failed to get file type"))?;
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else {
            has_files = true;
        }
    }

    if subdirs.len() == 1 && !has_files {
        return Ok(subdirs.into_iter().next().unwrap());
    }

    Ok(src_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// sha256 of b"raw binary payload"
    const RAW_SHA: &str = "55ab04cc83585c8d00e274c6be47cbc90807e2da33570ef2d162404e2a587b83";

    #[test]
    fn file_name_uses_url_basename() {
        assert_eq!(
            source_file_name("https://example.com/dl/omp-darwin-arm64", "omp"),
            "omp-darwin-arm64"
        );
    }

    #[test]
    fn file_name_drops_query_and_fragment() {
        assert_eq!(
            source_file_name("https://example.com/dl/tool?v=1#top", "tool"),
            "tool"
        );
    }

    #[test]
    fn file_name_falls_back_to_formula_name() {
        assert_eq!(source_file_name("https://example.com/bin/", "tool"), "tool");
        assert_eq!(source_file_name("https://example.com", "tool"), "tool");
    }

    #[test]
    fn file_name_rejects_traversal_segments() {
        assert_eq!(source_file_name("https://example.com/..", "tool"), "tool");
        assert_eq!(
            source_file_name("https://example.com/a/../", "tool"),
            "tool"
        );
    }

    /// Serve a 200 response whose body arrives in `chunks` pieces, sleeping
    /// `gap` before each one. With `stall`, send the headers then hang.
    async fn trickle_server(chunks: usize, gap: Duration, stall: bool) -> String {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {chunks}\r\n\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                    if stall {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    for _ in 0..chunks {
                        tokio::time::sleep(gap).await;
                        if sock.write_all(b"x").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}/src")
    }

    #[tokio::test]
    async fn slow_but_steady_download_outlives_idle_timeout() {
        // 20 chunks * 50ms = ~1s total, well over the 400ms idle limit,
        // but no single gap exceeds it.
        let url = trickle_server(20, Duration::from_millis(50), false).await;
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");

        download_source_with(
            &url,
            &dest,
            Duration::from_secs(5),
            Duration::from_millis(400),
        )
        .await
        .expect("steady slow transfer must not time out");

        assert_eq!(std::fs::read(&dest).unwrap(), b"x".repeat(20));
    }

    #[tokio::test]
    async fn stalled_download_fails_with_timeout_error() {
        let url = trickle_server(10, Duration::from_millis(1), true).await;
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");

        let started = std::time::Instant::now();
        let err = download_source_with(
            &url,
            &dest,
            Duration::from_secs(5),
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {err}"
        );
    }

    #[tokio::test]
    async fn nounzip_stages_raw_file_without_extraction() {
        let mock_server = MockServer::start().await;
        let payload = b"raw binary payload";

        Mock::given(method("GET"))
            .and(path("/dl/omp-darwin-arm64"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.to_vec()))
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/dl/omp-darwin-arm64", mock_server.uri());

        let src_root = download_and_extract_source("omp", &url, Some(RAW_SHA), tmp.path(), true)
            .await
            .expect("nounzip source should stage");

        assert_eq!(src_root, tmp.path().join("src"));
        let staged = src_root.join("omp-darwin-arm64");
        assert_eq!(std::fs::read(&staged).unwrap(), payload);
        assert!(!tmp.path().join("source.tar.gz").exists());
    }

    #[tokio::test]
    async fn nounzip_checksum_mismatch_still_fails() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/dl/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"raw binary payload"))
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/dl/tool", mock_server.uri());
        let wrong = "0".repeat(64);

        let err = download_and_extract_source("tool", &url, Some(&wrong), tmp.path(), true)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }));
    }

    #[tokio::test]
    async fn non_archive_without_nounzip_is_unsupported() {
        let mock_server = MockServer::start().await;
        // Mach-O magic: not a recognized archive.
        let mut payload = vec![0xcf, 0xfa, 0xed, 0xfe];
        payload.extend_from_slice(b"raw binary payload");
        let sha = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(&payload);
            crate::checksum::sha256_hex(h)
        };

        Mock::given(method("GET"))
            .and(path("/dl/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload))
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let url = format!("{}/dl/tool", mock_server.uri());

        let err = download_and_extract_source("tool", &url, Some(&sha), tmp.path(), false)
            .await
            .unwrap_err();
        match err {
            Error::UnsupportedFormula { name, reason } => {
                assert_eq!(name, "tool");
                assert!(
                    reason.contains("unsupported archive format"),
                    "reason should explain the real problem: {reason}"
                );
            }
            other => panic!("expected UnsupportedFormula, got {other:?}"),
        }
    }
}
