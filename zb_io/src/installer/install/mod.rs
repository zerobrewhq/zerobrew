mod bottle;
pub mod doctor;
mod outdated;
mod plan;
mod source;
mod uninstall;
mod upgrade;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::cellar::link::Linker;
use crate::cellar::materialize::Cellar;
use crate::network::api::ApiClient;
use crate::network::cache::ApiCache;
use crate::network::download::{DownloadProgressCallback, DownloadRequest, ParallelDownloader};
use crate::progress::{InstallProgress, ProgressCallback};
use crate::storage::blob::BlobCache;
use crate::storage::db::Database;
use crate::storage::store::Store;

use zb_core::{ConcurrencyLimits, Error, Formula, InstallMethod};

use bottle::{BottleJob, dependency_cellar_path, prepare_bottle};

const MAX_CORRUPTION_RETRIES: usize = 3;

/// Acquire the cross-process install lock. The returned `File` must be kept
/// alive (e.g. `let _lock = ...`) for the duration the lock should be held —
/// dropping it releases the flock. Re-acquiring in the same process while the
/// guard is alive would deadlock, so multi-step flows (e.g. `upgrade`) take
/// the lock once and call the no-lock `execute_inner` directly.
pub(crate) fn acquire_install_lock(locks_dir: &Path) -> Result<File, Error> {
    let lock_path = locks_dir.join("install.lock");
    let lock_file =
        File::create(&lock_path).map_err(Error::store("failed to create install lock"))?;
    lock_file
        .lock()
        .map_err(Error::store("failed to acquire install lock"))?;
    Ok(lock_file)
}

pub struct Installer {
    api_client: ApiClient,
    downloader: ParallelDownloader,
    store: Store,
    cellar: Cellar,
    linker: Linker,
    pub(crate) db: Database,
    prefix: PathBuf,
    locks_dir: PathBuf,
}

#[derive(Debug)]
pub struct PlannedInstall {
    pub install_name: String,
    pub formula: Formula,
    pub method: InstallMethod,
}

#[derive(Debug)]
pub struct InstallPlan {
    pub items: Vec<PlannedInstall>,
}

#[derive(Debug)]
pub struct PlanFailure {
    pub name: String,
    pub error: Error,
}

#[derive(Debug)]
pub struct ExecuteResult {
    pub installed: usize,
}

/// A package that has a newer version available upstream.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OutdatedPackage {
    pub name: String,
    pub installed_version: String,
    pub current_version: String,
    #[serde(skip)]
    pub installed_sha256: String,
    #[serde(skip)]
    pub current_sha256: String,
    #[serde(skip)]
    pub is_source_build: bool,
}

impl Installer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_client: ApiClient,
        blob_cache: BlobCache,
        store: Store,
        cellar: Cellar,
        linker: Linker,
        db: Database,
        prefix: PathBuf,
        locks_dir: PathBuf,
    ) -> Self {
        Self {
            api_client,
            downloader: ParallelDownloader::new(blob_cache),
            store,
            cellar,
            linker,
            db,
            prefix,
            locks_dir,
        }
    }

    /// Fetch the formula index again and drop cached API responses.
    /// Returns how many formulas the fresh index has.
    pub async fn update_api_cache(&self) -> Result<usize, Error> {
        self.api_client.clear_cache()?;
        self.api_client.refresh_index(true).await?;
        Ok(self.api_client.index_len())
    }

    pub async fn execute(&mut self, plan: InstallPlan, link: bool) -> Result<ExecuteResult, Error> {
        self.execute_with_progress(plan, link, None).await
    }

    pub async fn execute_with_progress(
        &mut self,
        plan: InstallPlan,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<ExecuteResult, Error> {
        let _lock = acquire_install_lock(&self.locks_dir)?;
        self.execute_inner(plan, link, progress).await
    }

    /// No-lock variant of `execute_with_progress`. Callers MUST already hold
    /// the install lock — used by `upgrade` to compose uninstall + install
    /// under a single lock acquisition.
    pub(crate) async fn execute_inner(
        &mut self,
        plan: InstallPlan,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<ExecuteResult, Error> {
        let report = |event: InstallProgress| {
            if let Some(ref cb) = progress {
                cb(event);
            }
        };

        let (bottle_items, source_items): (Vec<_>, Vec<_>) = plan
            .items
            .into_iter()
            .partition(|item| matches!(item.method, InstallMethod::Bottle(_)));

        if bottle_items.is_empty() && source_items.is_empty() {
            return Ok(ExecuteResult { installed: 0 });
        }

        let mut installed = 0usize;
        let mut error: Option<Error> = None;

        if !bottle_items.is_empty() {
            let requests: Vec<DownloadRequest> = bottle_items
                .iter()
                .map(|item| {
                    let InstallMethod::Bottle(ref bottle) = item.method else {
                        unreachable!()
                    };
                    DownloadRequest {
                        url: bottle.url.clone(),
                        sha256: bottle.sha256.clone(),
                        name: item.formula.name.clone(),
                    }
                })
                .collect();

            let download_progress: Option<DownloadProgressCallback> = progress.clone().map(|cb| {
                Arc::new(move |event: InstallProgress| {
                    cb(event);
                }) as DownloadProgressCallback
            });

            let mut rx = self
                .downloader
                .download_streaming(requests, download_progress.clone());

            // Bottles are unpacked and relocated as they arrive, several at
            // a time, while the rest are still downloading. Only the final
            // step (clone into the Cellar, record, link) runs here, one
            // bottle at a time.
            let prepare_limit = Arc::new(Semaphore::new(ConcurrencyLimits::default().materialize));
            let mut preparing = FuturesUnordered::new();
            let mut downloads_open = true;

            while downloads_open || !preparing.is_empty() {
                tokio::select! {
                    result = rx.recv(), if downloads_open => match result {
                        None => downloads_open = false,
                        Some(Err(e)) => error = Some(e),
                        Some(Ok(download)) => {
                            let job = BottleJob::new(download.index, &bottle_items[download.index]);
                            report(InstallProgress::UnpackStarted {
                                name: job.formula_name.clone(),
                            });
                            let store = self.store.clone();
                            let cellar = self.cellar.clone();
                            let downloader = self.downloader.clone();
                            let progress = download_progress.clone();
                            let limit = prepare_limit.clone();
                            preparing.push(tokio::spawn(async move {
                                let _permit = limit.acquire_owned().await;
                                let index = job.index;
                                let outcome = prepare_bottle(
                                    store,
                                    cellar,
                                    downloader,
                                    job,
                                    download.blob_path,
                                    progress,
                                )
                                .await;
                                (index, outcome)
                            }));
                        }
                    },
                    Some(joined) = preparing.next() => match joined {
                        Ok((index, Ok(entry))) => {
                            match self.finish_bottle_item(&bottle_items[index], &entry, link, &report) {
                                Ok(()) => installed += 1,
                                Err(e) => error = Some(e),
                            }
                        }
                        Ok((_, Err(e))) => error = Some(e),
                        Err(e) => {
                            error = Some(Error::ExecutionError {
                                message: format!("bottle preparation task failed: {e}"),
                            })
                        }
                    },
                }
            }
        }

        for item in &source_items {
            let InstallMethod::Source(ref build_plan) = item.method else {
                unreachable!()
            };

            report(InstallProgress::UnpackStarted {
                name: item.formula.name.clone(),
            });

            match self
                .install_from_source(item, build_plan, link, &report)
                .await
            {
                Ok(()) => installed += 1,
                Err(e) => {
                    error = Some(e);
                    continue;
                }
            }
        }

        if let Some(e) = error {
            return Err(e);
        }

        Ok(ExecuteResult { installed })
    }

    pub async fn install(&mut self, names: &[String], link: bool) -> Result<ExecuteResult, Error> {
        let (casks, formulas): (Vec<_>, Vec<_>) = names
            .iter()
            .cloned()
            .partition(|name| name.starts_with("cask:"));

        let mut installed = 0usize;

        if !formulas.is_empty() {
            let plan = self.plan(&formulas).await?;
            installed += self.execute(plan, link).await?.installed;
        }

        if !casks.is_empty() {
            installed += self.install_casks(&casks, link).await?.installed;
        }

        Ok(ExecuteResult { installed })
    }

    pub async fn install_casks(
        &mut self,
        names: &[String],
        link: bool,
    ) -> Result<ExecuteResult, Error> {
        let mut installed = 0usize;
        for name in names {
            let token = name
                .strip_prefix("cask:")
                .expect("install_casks expects cask: prefixed names");
            self.install_single_cask(token, link).await?;
            installed += 1;
        }
        Ok(ExecuteResult { installed })
    }

    pub fn is_installed(&self, name: &str) -> bool {
        self.db.get_installed(name).is_some()
    }

    pub fn get_installed(&self, name: &str) -> Option<crate::storage::db::InstalledKeg> {
        self.db.get_installed(name)
    }

    pub fn list_installed(&self) -> Result<Vec<crate::storage::db::InstalledKeg>, Error> {
        self.db.list_installed()
    }

    pub fn keg_path(&self, name: &str, version: &str) -> PathBuf {
        self.cellar.keg_path(name, version)
    }

    fn cleanup_materialized(cellar: &Cellar, name: &str, version: &str) {
        if let Err(e) = cellar.remove_keg(name, version) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove keg after install error"
            );
        }
    }
}

pub fn create_installer(
    root: &Path,
    prefix: &Path,
    concurrency: usize,
) -> Result<Installer, Error> {
    if !root.exists() {
        fs::create_dir_all(root).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::StoreCorruption {
                    message: format!(
                        "cannot create root directory '{}': permission denied.\n\n\
                        Create it with:\n  sudo mkdir -p {} && sudo chown $USER {}",
                        root.display(),
                        root.display(),
                        root.display()
                    ),
                }
            } else {
                Error::StoreCorruption {
                    message: format!("failed to create root directory '{}': {e}", root.display()),
                }
            }
        })?;
    }

    fs::create_dir_all(root.join("db")).map_err(Error::store("failed to create db directory"))?;

    fs::create_dir_all(root.join("cache"))
        .map_err(Error::store("failed to create cache directory"))?;

    let api_cache_path = root.join("cache/api-cache.sqlite");
    let api_cache =
        ApiCache::open(&api_cache_path).map_err(Error::store("failed to open API cache"))?;

    let api_client = match std::env::var("ZEROBREW_API_URL") {
        Ok(url) => ApiClient::with_base_url(url)?,
        Err(_) => ApiClient::new(),
    }
    .with_cache(api_cache);

    let blob_cache =
        BlobCache::new(&root.join("cache")).map_err(Error::store("failed to create blob cache"))?;
    let store = Store::new(root).map_err(Error::store("failed to create store"))?;
    // Use prefix/Cellar so bottles' hardcoded rpaths work
    let cellar =
        Cellar::new_at(prefix.join("Cellar")).map_err(Error::store("failed to create cellar"))?;
    let linker = Linker::new(prefix).map_err(Error::store("failed to create linker"))?;
    let db = Database::open(&root.join("db/zb.sqlite3"))?;

    let locks_dir = root.join("locks");
    fs::create_dir_all(&locks_dir).map_err(Error::store("failed to create locks directory"))?;

    let parallel_downloader = ParallelDownloader::with_concurrency(blob_cache, concurrency);

    Ok(Installer {
        api_client,
        downloader: parallel_downloader,
        store,
        cellar,
        linker,
        db,
        prefix: prefix.to_path_buf(),
        locks_dir,
    })
}

#[cfg(test)]
mod test_support {
    pub fn create_bottle_tarball(formula_name: &str) -> Vec<u8> {
        create_bottle_tarball_with_version(formula_name, "1.0.0")
    }

    pub fn create_bottle_tarball_with_version(formula_name: &str, version: &str) -> Vec<u8> {
        create_bottle_tarball_with_files(formula_name, version, &[])
    }

    /// A bottle with `bin/<name>` plus extra `(path in keg, contents)` files.
    pub fn create_bottle_tarball_with_files(
        formula_name: &str,
        version: &str,
        extra_files: &[(&str, &str)],
    ) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        use tar::Builder;

        let mut builder = Builder::new(Vec::new());

        let content = format!("#!/bin/sh\necho {} v{}", formula_name, version);
        let files = std::iter::once((format!("bin/{formula_name}"), content.as_str(), 0o755))
            .chain(
                extra_files
                    .iter()
                    .map(|(path, contents)| (path.to_string(), *contents, 0o644)),
            );

        for (path, contents, mode) in files {
            let mut header = tar::Header::new_gnu();
            header
                .set_path(format!("{}/{}/{}", formula_name, version, path))
                .unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append(&header, contents.as_bytes()).unwrap();
        }

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    pub fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        crate::checksum::sha256_hex(hasher)
    }

    pub fn get_test_bottle_tag() -> &'static str {
        if cfg!(target_os = "linux") {
            "x86_64_linux"
        } else if cfg!(target_arch = "x86_64") {
            "sonoma"
        } else {
            "arm64_sonoma"
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    use super::test_support::*;

    #[tokio::test]
    async fn install_completes_successfully() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("testpkg");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "testpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["testpkg".to_string()], true)
            .await
            .unwrap();

        assert!(root.join("cellar/testpkg/1.0.0").exists());
        assert!(prefix.join("bin/testpkg").exists());

        let installed = installer.db.get_installed("testpkg");
        assert!(installed.is_some());
        assert_eq!(installed.unwrap().version, "1.0.0");
    }

    #[tokio::test]
    async fn install_with_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let dep_bottle = create_bottle_tarball("deplib");
        let dep_sha = sha256_hex(&dep_bottle);
        let main_bottle = create_bottle_tarball("mainpkg");
        let main_sha = sha256_hex(&main_bottle);

        let tag = get_test_bottle_tag();
        let dep_json = format!(
            r#"{{"name":"deplib","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/deplib-1.0.0.{}.bottle.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            tag,
            dep_sha
        );
        let main_json = format!(
            r#"{{"name":"mainpkg","versions":{{"stable":"2.0.0"}},"dependencies":["deplib"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mainpkg-2.0.0.{}.bottle.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            tag,
            main_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/deplib.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&dep_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/mainpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&main_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/deplib-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(dep_bottle))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/mainpkg-2.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(main_bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["mainpkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("mainpkg").is_some());
        assert!(installer.db.get_installed("deplib").is_some());
    }

    #[tokio::test]
    async fn preserves_successful_installs_when_one_package_fails() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let good_bottle = create_bottle_tarball("goodpkg");
        let good_sha = sha256_hex(&good_bottle);

        let tag = get_test_bottle_tag();
        let good_json = format!(
            r#"{{
                "name": "goodpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            good_sha
        );

        let bad_json = format!(
            r#"{{
                "name": "badpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/badpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        Mock::given(method("GET"))
            .and(path("/formula/goodpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&good_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/badpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&bad_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(good_bottle))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/badpkg-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_delay(Duration::from_millis(100))
                    .set_body_string("download failed"),
            )
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let result = installer
            .install(&["goodpkg".to_string(), "badpkg".to_string()], false)
            .await;
        assert!(result.is_err());

        assert!(installer.db.get_installed("goodpkg").is_some());
        assert!(installer.db.get_installed("badpkg").is_none());
        assert!(root.join("cellar/goodpkg/1.0.0").exists());
    }

    /// An installer backed by a mock API serving `name` 1.0.0 as `bottle`.
    async fn installer_for_bottle(
        mock_server: &MockServer,
        tmp: &TempDir,
        name: &str,
        bottle: Vec<u8>,
    ) -> (Installer, std::path::PathBuf, std::path::PathBuf) {
        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{ "stable": {{ "files": {{
                    "{tag}": {{
                        "url": "{uri}/bottles/{name}-1.0.0.{tag}.bottle.tar.gz",
                        "sha256": "{sha}"
                    }}
                }} }} }}
            }}"#,
            uri = mock_server.uri(),
            sha = sha256_hex(&bottle),
        );

        Mock::given(method("GET"))
            .and(path(format!("/formula/{name}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/{name}-1.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let installer = Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/zb.sqlite3")).unwrap(),
            prefix.clone(),
            root.join("locks"),
        );
        (installer, root, prefix)
    }

    #[tokio::test]
    async fn install_copies_bottle_etc_files_into_prefix() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball_with_files(
            "withconf",
            "1.0.0",
            &[(".bottle/etc/withconf/withconf.conf", "setting = 1\n")],
        );
        let (mut installer, _root, prefix) =
            installer_for_bottle(&mock_server, &tmp, "withconf", bottle).await;

        installer
            .install(&["withconf".to_string()], true)
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(prefix.join("etc/withconf/withconf.conf")).unwrap(),
            "setting = 1\n"
        );
    }

    #[tokio::test]
    async fn etc_install_failure_fails_install_and_cleans_keg() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let bottle = create_bottle_tarball_with_files(
            "badconf",
            "1.0.0",
            &[(".bottle/etc/badconf/badconf.conf", "setting = 1\n")],
        );
        let (mut installer, root, prefix) =
            installer_for_bottle(&mock_server, &tmp, "badconf", bottle).await;

        // A file where the config directory needs to go.
        fs::write(prefix.join("etc/badconf"), "in the way").unwrap();

        let result = installer.install(&["badconf".to_string()], true).await;

        assert!(result.is_err());
        assert!(installer.db.get_installed("badconf").is_none());
        assert!(!root.join("cellar/badconf/1.0.0").exists());
        assert!(!prefix.join("bin/badconf").exists());
    }

    #[tokio::test]
    async fn db_persist_failure_cleans_materialized_and_linked_files() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("rollbackme");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "rollbackme",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/rollbackme-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/rollbackme.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/rollbackme-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let db_path = root.join("db/zb.sqlite3");
        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&db_path).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TABLE installed_kegs", []).unwrap();

        let result = installer.install(&["rollbackme".to_string()], true).await;
        assert!(result.is_err());

        assert!(!root.join("cellar/rollbackme/1.0.0").exists());
        assert!(!prefix.join("bin/rollbackme").exists());
        assert!(!prefix.join("opt/rollbackme").exists());
        assert!(root.join("store").join(&bottle_sha).exists());
    }

    #[tokio::test]
    async fn db_persist_failure_cleans_materialized_tap_formula_keg() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("terraform");
        let bottle_sha = sha256_hex(&bottle);
        let tag = get_test_bottle_tag();

        let tap_formula_rb = format!(
            r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "{}/v2/hashicorp/tap"
    sha256 cellar: :any_skip_relocation, {}: "{}"
  end
end
"#,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/hashicorp/tap/terraform/blobs/sha256:{bottle_sha}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let db_path = root.join("db/zb.sqlite3");
        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&db_path).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TABLE installed_kegs", []).unwrap();

        let result = installer
            .install(&["hashicorp/tap/terraform".to_string()], true)
            .await;
        assert!(result.is_err());

        assert!(!root.join("cellar/terraform/1.10.0").exists());
        assert!(!prefix.join("bin/terraform").exists());
        assert!(!prefix.join("opt/terraform").exists());
        assert!(root.join("store").join(&bottle_sha).exists());
    }

    #[tokio::test]
    async fn parallel_api_fetching_with_deep_deps() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let leaf1_bottle = create_bottle_tarball("leaf1");
        let leaf1_sha = sha256_hex(&leaf1_bottle);
        let leaf2_bottle = create_bottle_tarball("leaf2");
        let leaf2_sha = sha256_hex(&leaf2_bottle);
        let mid1_bottle = create_bottle_tarball("mid1");
        let mid1_sha = sha256_hex(&mid1_bottle);
        let mid2_bottle = create_bottle_tarball("mid2");
        let mid2_sha = sha256_hex(&mid2_bottle);
        let root_bottle = create_bottle_tarball("root");
        let root_sha = sha256_hex(&root_bottle);

        let tag = get_test_bottle_tag();
        let leaf1_json = format!(
            r#"{{"name":"leaf1","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/leaf1.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            leaf1_sha
        );
        let leaf2_json = format!(
            r#"{{"name":"leaf2","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/leaf2.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            leaf2_sha
        );
        let mid1_json = format!(
            r#"{{"name":"mid1","versions":{{"stable":"1.0.0"}},"dependencies":["leaf1"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mid1.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            mid1_sha
        );
        let mid2_json = format!(
            r#"{{"name":"mid2","versions":{{"stable":"1.0.0"}},"dependencies":["leaf1","leaf2"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mid2.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            mid2_sha
        );
        let root_json = format!(
            r#"{{"name":"root","versions":{{"stable":"1.0.0"}},"dependencies":["mid1","mid2"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/root.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            root_sha
        );

        for (name, json) in [
            ("leaf1", &leaf1_json),
            ("leaf2", &leaf2_json),
            ("mid1", &mid1_json),
            ("mid2", &mid2_json),
            ("root", &root_json),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{}.json", name)))
                .respond_with(ResponseTemplate::new(200).set_body_string(json))
                .mount(&mock_server)
                .await;
        }
        for (name, bottle) in [
            ("leaf1", &leaf1_bottle),
            ("leaf2", &leaf2_bottle),
            ("mid1", &mid1_bottle),
            ("mid2", &mid2_bottle),
            ("root", &root_bottle),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/bottles/{}.tar.gz", name)))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
                .mount(&mock_server)
                .await;
        }

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["root".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("root").is_some());
        assert!(installer.db.get_installed("mid1").is_some());
        assert!(installer.db.get_installed("mid2").is_some());
        assert!(installer.db.get_installed("leaf1").is_some());
        assert!(installer.db.get_installed("leaf2").is_some());
    }

    #[tokio::test]
    async fn streaming_extraction_processes_as_downloads_complete() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let fast_bottle = create_bottle_tarball("fastpkg");
        let fast_sha = sha256_hex(&fast_bottle);
        let slow_bottle = create_bottle_tarball("slowpkg");
        let slow_sha = sha256_hex(&slow_bottle);

        let tag = get_test_bottle_tag();
        let fast_json = format!(
            r#"{{"name":"fastpkg","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/fast.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            fast_sha
        );
        let slow_json = format!(
            r#"{{"name":"slowpkg","versions":{{"stable":"1.0.0"}},"dependencies":["fastpkg"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/slow.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            slow_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/fastpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&fast_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/slowpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&slow_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/fast.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fast_bottle.clone()))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/slow.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(slow_bottle.clone())
                    .set_delay(Duration::from_millis(100)),
            )
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["slowpkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("fastpkg").is_some());
        assert!(installer.db.get_installed("slowpkg").is_some());
        assert!(root.join("cellar/fastpkg/1.0.0").exists());
        assert!(root.join("cellar/slowpkg/1.0.0").exists());
        assert!(prefix.join("bin/fastpkg").exists());
        assert!(prefix.join("bin/slowpkg").exists());
    }

    #[tokio::test]
    async fn a_corrupt_bottle_fails_alone_after_its_retries() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let good_bottle = create_bottle_tarball("goodpkg");
        // Right checksum, but not a tarball: unpacking fails every time.
        let corrupt_bottle = b"this is not a gzip stream".to_vec();

        let tag = get_test_bottle_tag();
        let formula = |name: &str, sha: &str| {
            format!(
                r#"{{"name":"{name}","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}/bottles/{name}.tar.gz","sha256":"{sha}"}}}}}}}}}}"#,
                mock_server.uri()
            )
        };
        for (name, bottle) in [("goodpkg", &good_bottle), ("corruptpkg", &corrupt_bottle)] {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{name}.json")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(formula(name, &sha256_hex(bottle))),
                )
                .mount(&mock_server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/bottles/{name}.tar.gz")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
                .mount(&mock_server)
                .await;
        }

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();
        let mut installer = Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/zb.sqlite3")).unwrap(),
            prefix.clone(),
            root.join("locks"),
        );

        let err = installer
            .install(&["goodpkg".to_string(), "corruptpkg".to_string()], true)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("attempts"), "{err}");
        assert!(installer.db.get_installed("goodpkg").is_some());
        assert!(prefix.join("bin/goodpkg").exists());
        assert!(installer.db.get_installed("corruptpkg").is_none());
        assert!(!root.join("cellar/corruptpkg").exists());
        let corrupt_downloads = mock_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/bottles/corruptpkg.tar.gz")
            .count();
        assert_eq!(corrupt_downloads, super::MAX_CORRUPTION_RETRIES);
        assert!(
            !root
                .join("cache/blobs")
                .join(format!("{}.tar.gz", sha256_hex(&corrupt_bottle)))
                .exists(),
            "the corrupt blob must not be kept"
        );
    }

    #[tokio::test]
    async fn retries_on_corrupted_download() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("retrypkg");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "retrypkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/retrypkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/retrypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        let attempt_count = Arc::new(AtomicUsize::new(0));
        let attempt_clone = attempt_count.clone();
        let valid_bottle = bottle.clone();

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/retrypkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(move |_: &wiremock::Request| {
                let _attempt = attempt_clone.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_bytes(valid_bottle.clone())
            })
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["retrypkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.is_installed("retrypkg"));
        assert!(root.join("cellar/retrypkg/1.0.0").exists());
        assert!(prefix.join("bin/retrypkg").exists());
    }

    #[tokio::test]
    async fn fails_after_max_retries() {
        // Validates the retry mechanism structure -- proper integration test
        // would need injection of corruption between download and extraction.
    }
}
