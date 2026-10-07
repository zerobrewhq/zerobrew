use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;
use zb_core::{Error, InstallMethod, formula_token};

use crate::cellar::link::Linker;
use crate::cellar::materialize::Cellar;
use crate::installer::cask::resolve_cask;
use crate::network::download::{DownloadProgressCallback, DownloadRequest, ParallelDownloader};
use crate::progress::InstallProgress;
use crate::storage::store::Store;

use super::{Installer, MAX_CORRUPTION_RETRIES, PlannedInstall};

impl Installer {
    /// Clone a prepared store entry into the Cellar, record it, and link it.
    /// Runs on the install task, one bottle at a time, once
    /// [`prepare_bottle`] has produced the entry.
    pub(super) fn finish_bottle_item(
        &mut self,
        item: &PlannedInstall,
        store_entry: &Path,
        link: bool,
        report: &impl Fn(InstallProgress),
    ) -> Result<(), Error> {
        let InstallMethod::Bottle(ref bottle) = item.method else {
            unreachable!()
        };
        let install_name = &item.install_name;
        let formula_name = &item.formula.name;
        let version = item.formula.effective_version();
        let store_key = &bottle.sha256;

        let keg_path = self
            .cellar
            .materialize(formula_name, &version, store_entry)?;

        // Before recording the install, so a failure doesn't leave the
        // package marked installed without its config files.
        crate::cellar::install_etc_var(&keg_path, &self.prefix)
            .map_err(Error::file("failed to install etc/var files"))
            .inspect_err(|_| {
                Self::cleanup_materialized(&self.cellar, formula_name, &version);
            })?;

        report(InstallProgress::UnpackCompleted {
            name: formula_name.clone(),
        });

        let tx = self.db.transaction().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        tx.record_install(install_name, &version, store_key)
            .inspect_err(|_| {
                Self::cleanup_materialized(&self.cellar, formula_name, &version);
            })?;

        tx.commit().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        let opt_names: Vec<String> = item
            .formula
            .aliases
            .iter()
            .chain(&item.formula.oldnames)
            .cloned()
            .collect();
        if let Err(e) = self.linker.link_opt(&keg_path, &opt_names) {
            warn!(formula = %install_name, error = %e, "failed to create opt link");
        }

        if link && !item.formula.is_keg_only() {
            report(InstallProgress::LinkStarted {
                name: formula_name.clone(),
            });
            match self.linker.link_keg(&keg_path) {
                Ok(linked_files) => {
                    report(InstallProgress::LinkCompleted {
                        name: formula_name.clone(),
                    });
                    self.record_linked_files(install_name, &version, &linked_files);
                }
                Err(e) => {
                    let _ = self.linker.unlink_keg(&keg_path);
                    report(InstallProgress::InstallCompleted {
                        name: formula_name.clone(),
                    });
                    return Err(e);
                }
            }
        } else if link && item.formula.is_keg_only() {
            let reason = match &item.formula.keg_only {
                zb_core::KegOnly::Reason(s) => s.clone(),
                _ if formula_name.contains('@') => "versioned formula".to_string(),
                _ => "keg-only formula".to_string(),
            };
            report(InstallProgress::LinkSkipped {
                name: formula_name.clone(),
                reason,
            });
        }

        report(InstallProgress::InstallCompleted {
            name: formula_name.clone(),
        });

        Ok(())
    }

    fn record_linked_files(
        &mut self,
        name: &str,
        version: &str,
        linked_files: &[crate::cellar::link::LinkedFile],
    ) {
        if let Ok(tx) = self.db.transaction() {
            let mut ok = true;
            for linked in linked_files {
                if tx
                    .record_linked_file(
                        name,
                        version,
                        &linked.link_path.to_string_lossy(),
                        &linked.target_path.to_string_lossy(),
                    )
                    .is_err()
                {
                    ok = false;
                    break;
                }
            }
            if ok {
                let _ = tx.commit();
            }
        }
    }

    pub(super) fn cleanup_failed_install(
        linker: &Linker,
        cellar: &Cellar,
        name: &str,
        version: &str,
        keg_path: &Path,
        unlink: bool,
    ) {
        if unlink && let Err(e) = linker.unlink_keg(keg_path) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to clean up links after install error"
            );
        }

        if let Err(e) = cellar.remove_keg(name, version) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove keg after install error"
            );
        }
    }

    pub(super) async fn install_single_cask(
        &mut self,
        token: &str,
        link: bool,
    ) -> Result<(), Error> {
        let cask_json = self.api_client.get_cask(token).await?;
        let cask = resolve_cask(token, &cask_json)?;

        let blob_path = self
            .downloader
            .download_single(
                DownloadRequest {
                    url: cask.url.clone(),
                    sha256: cask.sha256.clone(),
                    name: cask.install_name.clone(),
                },
                None,
            )
            .await?;

        let keg_path = self.cellar.keg_path(&cask.install_name, &cask.version);
        let mut cleanup = FailedInstallGuard::new(
            &self.linker,
            &self.cellar,
            &cask.install_name,
            &cask.version,
            &keg_path,
            link,
        );

        if crate::extraction::is_archive(&blob_path)? {
            // Cask archives are staged file by file; nothing to prepare.
            let extracted =
                self.store
                    .ensure_entry(&cask.sha256, &blob_path, "cask v1\n", |_| Ok(()))?;
            stage_cask_binaries(&extracted, &keg_path, &cask)?;
        } else {
            stage_raw_cask_binary(&blob_path, &keg_path, &cask)?;
        }

        let linked_files = if link {
            self.linker.link_keg(&keg_path)?
        } else {
            Vec::new()
        };

        let tx = self.db.transaction()?;
        tx.record_install(&cask.install_name, &cask.version, &cask.sha256)?;
        for linked in &linked_files {
            tx.record_linked_file(
                &cask.install_name,
                &cask.version,
                &linked.link_path.to_string_lossy(),
                &linked.target_path.to_string_lossy(),
            )?;
        }
        tx.commit()?;

        cleanup.disarm();
        Ok(())
    }
}

/// What it takes to turn a downloaded bottle into a store entry.
#[derive(Debug, Clone)]
pub(super) struct BottleJob {
    /// Position in the plan's bottle list.
    pub(super) index: usize,
    pub(super) formula_name: String,
    pub(super) version: String,
    pub(super) sha256: String,
    pub(super) url: String,
    pub(super) build_prefix: String,
}

impl BottleJob {
    pub(super) fn new(index: usize, item: &PlannedInstall) -> Self {
        let InstallMethod::Bottle(ref bottle) = item.method else {
            unreachable!()
        };
        Self {
            index,
            formula_name: item.formula.name.clone(),
            version: item.formula.effective_version(),
            sha256: bottle.sha256.clone(),
            url: bottle.url.clone(),
            build_prefix: bottle.build_prefix().to_string(),
        }
    }
}

/// Unpack and relocate a downloaded bottle into the store. The blocking
/// work runs off the async runtime, so several bottles can be prepared at
/// once while others are still downloading. A blob that turns out corrupt
/// is downloaded again, up to [`MAX_CORRUPTION_RETRIES`] times.
pub(super) async fn prepare_bottle(
    store: Store,
    cellar: Cellar,
    downloader: ParallelDownloader,
    job: BottleJob,
    mut blob_path: PathBuf,
    progress: Option<DownloadProgressCallback>,
) -> Result<PathBuf, Error> {
    let fingerprint = cellar.relocation_fingerprint(&job.build_prefix);
    let mut last_error = None;

    for attempt in 0..MAX_CORRUPTION_RETRIES {
        let (store, cellar, job_for_task, fingerprint, blob) = (
            store.clone(),
            cellar.clone(),
            job.clone(),
            fingerprint.clone(),
            blob_path.clone(),
        );
        let outcome = tokio::task::spawn_blocking(move || {
            let job = job_for_task;
            store.ensure_entry(&job.sha256, &blob, &fingerprint, |extracted| {
                cellar.relocate_extracted(
                    extracted,
                    &job.formula_name,
                    &job.version,
                    &job.build_prefix,
                )
            })
        })
        .await
        .map_err(|e| Error::ExecutionError {
            message: format!("bottle preparation task failed: {e}"),
        })?;

        match outcome {
            Ok(entry) => return Ok(entry),
            Err(Error::StoreCorruption { message }) => {
                downloader.remove_blob(&job.sha256);

                if attempt + 1 < MAX_CORRUPTION_RETRIES {
                    warn!(
                        formula = %job.formula_name,
                        attempt = attempt + 2,
                        max_retries = MAX_CORRUPTION_RETRIES,
                        "corrupted download detected; retrying"
                    );

                    let request = DownloadRequest {
                        url: job.url.clone(),
                        sha256: job.sha256.clone(),
                        name: job.formula_name.clone(),
                    };

                    match downloader.download_single(request, progress.clone()).await {
                        Ok(new_path) => blob_path = new_path,
                        Err(e) => {
                            last_error = Some(e);
                            break;
                        }
                    }
                } else {
                    last_error = Some(Error::StoreCorruption {
                        message: format!(
                            "{message}\n\nFailed after {MAX_CORRUPTION_RETRIES} attempts. The download may be corrupted at the source."
                        ),
                    });
                }
            }
            Err(e) => {
                last_error = Some(e);
                break;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| Error::StoreCorruption {
        message: "extraction failed with unknown error".to_string(),
    }))
}

pub(super) fn dependency_cellar_path(
    cellar: &Cellar,
    installed_name: &str,
    version: &str,
) -> String {
    cellar
        .keg_path(formula_token(installed_name), version)
        .display()
        .to_string()
}

struct FailedInstallGuard<'a> {
    linker: &'a Linker,
    cellar: &'a Cellar,
    name: &'a str,
    version: &'a str,
    keg_path: &'a Path,
    unlink: bool,
    armed: bool,
}

impl<'a> FailedInstallGuard<'a> {
    fn new(
        linker: &'a Linker,
        cellar: &'a Cellar,
        name: &'a str,
        version: &'a str,
        keg_path: &'a Path,
        unlink: bool,
    ) -> Self {
        Self {
            linker,
            cellar,
            name,
            version,
            keg_path,
            unlink,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FailedInstallGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            Installer::cleanup_failed_install(
                self.linker,
                self.cellar,
                self.name,
                self.version,
                self.keg_path,
                self.unlink,
            );
        }
    }
}

fn stage_cask_binaries(
    extracted_root: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    let bin_dir = keg_path.join("bin");
    fs::create_dir_all(&bin_dir).map_err(Error::store("failed to create cask bin dir"))?;

    for binary in &cask.binaries {
        let source = resolve_cask_source_path(extracted_root, cask, &binary.source)?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' binary source '{}' not found in archive",
                    cask.token, binary.source
                ),
            });
        }

        let target = bin_dir.join(&binary.target);
        if target.exists() {
            fs::remove_file(&target)
                .map_err(Error::store("failed to replace existing cask binary"))?;
        }

        fs::copy(&source, &target).map_err(|e| Error::StoreCorruption {
            message: format!("failed to stage cask binary '{}': {e}", binary.target),
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&target)
                .map_err(Error::store("failed to read staged cask binary metadata"))?
                .permissions();
            if perms.mode() & 0o111 == 0 {
                perms.set_mode(0o755);
                fs::set_permissions(&target, perms)
                    .map_err(Error::store("failed to make staged cask binary executable"))?;
            }
        }
    }

    Ok(())
}

fn stage_raw_cask_binary(
    blob_path: &Path,
    keg_path: &Path,
    cask: &crate::installer::cask::ResolvedCask,
) -> Result<(), Error> {
    if cask.binaries.len() != 1 {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' has {} binary artifacts but the download is a raw binary; expected exactly 1",
                cask.token,
                cask.binaries.len()
            ),
        });
    }

    let binary = &cask.binaries[0];
    let bin_dir = keg_path.join("bin");
    fs::create_dir_all(&bin_dir).map_err(Error::store("failed to create cask bin dir"))?;

    let target = bin_dir.join(&binary.target);
    if target.exists() {
        fs::remove_file(&target).map_err(Error::store("failed to replace existing cask binary"))?;
    }

    fs::copy(blob_path, &target).map_err(|e| Error::StoreCorruption {
        message: format!("failed to stage cask binary '{}': {e}", binary.target),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .map_err(Error::store("failed to make staged cask binary executable"))?;
    }

    Ok(())
}

fn resolve_cask_source_path(
    extracted_root: &Path,
    cask: &crate::installer::cask::ResolvedCask,
    source: &str,
) -> Result<std::path::PathBuf, Error> {
    if source.starts_with("$APPDIR") {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' uses APPDIR artifacts which are not supported yet",
                cask.token
            ),
        });
    }

    let mut normalized = source.to_string();
    let caskroom_prefix = format!("$HOMEBREW_PREFIX/Caskroom/{}/{}/", cask.token, cask.version);
    if let Some(stripped) = normalized.strip_prefix(&caskroom_prefix) {
        normalized = stripped.to_string();
    }

    let source_path = Path::new(&normalized);
    if source_path.is_absolute() {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' binary source '{}' must be a relative path",
                cask.token, source
            ),
        });
    }

    for component in source_path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' binary source '{}' cannot contain '..'",
                    cask.token, source
                ),
            });
        }
    }

    Ok(extracted_root.join(source_path))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::cellar::Cellar;
    use crate::storage::db::Database;

    use super::*;

    #[test]
    fn dependency_cellar_path_uses_formula_token_for_tap_name() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();
        let path = dependency_cellar_path(&cellar, "hashicorp/tap/terraform", "1.10.0");

        assert!(path.ends_with("cellar/terraform/1.10.0"));
    }

    #[test]
    fn dependency_cellar_path_keeps_core_formula_name() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();
        let path = dependency_cellar_path(&cellar, "openssl@3", "3.3.2");

        assert!(path.ends_with("cellar/openssl@3/3.3.2"));
    }

    #[test]
    fn dependency_cellar_path_uses_name_from_db_record() {
        let tmp = TempDir::new().unwrap();
        let cellar = Cellar::new(tmp.path()).unwrap();

        let db_path = tmp.path().join("zb.sqlite3");
        let mut db = Database::open(&db_path).unwrap();
        let tx = db.transaction().unwrap();
        tx.record_install("hashicorp/tap/terraform", "1.10.0", "store-key")
            .unwrap();
        tx.commit().unwrap();

        let keg = db.get_installed("hashicorp/tap/terraform").unwrap();
        let path = dependency_cellar_path(&cellar, &keg.name, &keg.version);

        assert!(path.ends_with("cellar/terraform/1.10.0"));
    }

    #[test]
    fn stage_raw_cask_binary_copies_and_marks_executable() {
        let tmp = TempDir::new().unwrap();
        let blob_path = tmp.path().join("claude");
        fs::write(&blob_path, b"#!/bin/sh\necho hello").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:claude-code".to_string(),
            token: "claude-code".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/claude".to_string(),
            sha256: "aaa".to_string(),
            binaries: vec![crate::installer::cask::CaskBinary {
                source: "claude".to_string(),
                target: "claude".to_string(),
            }],
        };

        stage_raw_cask_binary(&blob_path, &keg_path, &cask).unwrap();

        let target = keg_path.join("bin/claude");
        assert!(target.exists());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "#!/bin/sh\necho hello"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755);
        }
    }

    #[test]
    fn stage_raw_cask_binary_rejects_multiple_binaries() {
        let tmp = TempDir::new().unwrap();
        let blob_path = tmp.path().join("blob");
        fs::write(&blob_path, b"data").unwrap();

        let keg_path = tmp.path().join("keg");
        let cask = crate::installer::cask::ResolvedCask {
            install_name: "cask:multi".to_string(),
            token: "multi".to_string(),
            version: "1.0.0".to_string(),
            url: "https://example.com/multi".to_string(),
            sha256: "bbb".to_string(),
            binaries: vec![
                crate::installer::cask::CaskBinary {
                    source: "a".to_string(),
                    target: "a".to_string(),
                },
                crate::installer::cask::CaskBinary {
                    source: "b".to_string(),
                    target: "b".to_string(),
                },
            ],
        };

        let err = stage_raw_cask_binary(&blob_path, &keg_path, &cask).unwrap_err();
        assert!(err.to_string().contains("raw binary"));
    }
}
