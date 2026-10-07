use std::sync::Arc;

use zb_core::{Error, InstallMethod};

use super::{InstallPlan, Installer, acquire_install_lock};
use crate::network::download::{DownloadProgressCallback, DownloadRequest};
use crate::progress::{InstallProgress, ProgressCallback};

impl Installer {
    /// Upgrade an installed package to its latest version.
    ///
    /// Bottle artifacts for the new version are fetched into the blob cache
    /// *before* the old keg is removed, so a download failure leaves the
    /// existing installation intact. Source-built upgrades have no
    /// equivalent pre-build stage and still go through uninstall-first.
    ///
    /// Uninstall-first on the link step is required either way: a fresh
    /// install would hit `LinkConflict` on the old version's symlinks and
    /// leave the old cellar directory behind on disk (the leak this method
    /// exists to fix).
    ///
    /// Returns `Ok(())` when the package is already on its latest version,
    /// `Error::NotInstalled` when there is no existing installation.
    pub async fn upgrade(
        &mut self,
        name: &str,
        build_from_source: bool,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<(), Error> {
        // One lock for the entire flow — uninstall + install must not race
        // with other zb processes touching the same package.
        let _lock = acquire_install_lock(&self.locks_dir)?;

        let old = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;

        // `plan_with_options` doesn't consult the installed DB, so an
        // empty-plan check wouldn't fire on already-current packages.
        if self.is_outdated(name).await?.is_none() {
            return Ok(());
        }

        let plan = self
            .plan_with_options(&[name.to_string()], build_from_source)
            .await?;

        // Fetch new bottles before touching the old install — a download
        // failure here leaves the existing keg intact.
        self.prefetch_plan_bottles(&plan, progress.clone()).await?;

        self.uninstall_by_version(name, &old.version)?;

        // We already hold the lock, so call the no-lock variant.
        self.execute_inner(plan, link, progress).await?;

        Ok(())
    }

    /// Pre-download bottle artifacts in `plan` into the blob cache. No-op
    /// for source-only plans.
    async fn prefetch_plan_bottles(
        &self,
        plan: &InstallPlan,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<(), Error> {
        let requests: Vec<DownloadRequest> = plan
            .items
            .iter()
            .filter_map(|item| match &item.method {
                InstallMethod::Bottle(bottle) => Some(DownloadRequest::new(
                    bottle.url.clone(),
                    bottle.sha256.clone(),
                    item.formula.name.clone(),
                )),
                _ => None,
            })
            .collect();

        if requests.is_empty() {
            return Ok(());
        }

        let download_progress: Option<DownloadProgressCallback> = progress.map(|cb| {
            Arc::new(move |event: InstallProgress| {
                cb(event);
            }) as DownloadProgressCallback
        });

        let mut rx = self
            .downloader
            .download_streaming(requests, download_progress);
        while let Some(result) = rx.recv().await {
            result?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::installer::install::test_support::*;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    fn formula_json(mock_uri: &str, name: &str, version: &str, tag: &str, sha: &str) -> String {
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "{version}" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{tag}": {{
                                "url": "{mock_uri}/bottles/{name}-{version}.{tag}.bottle.tar.gz",
                                "sha256": "{sha}"
                            }}
                        }}
                    }}
                }}
            }}"#
        )
    }

    fn make_installer(
        root: &std::path::Path,
        prefix: &std::path::Path,
        mock_uri: &str,
    ) -> Installer {
        fs::create_dir_all(root.join("db")).unwrap();
        let api_client = ApiClient::with_base_url(format!("{mock_uri}/formula")).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(root).unwrap();
        let cellar = Cellar::new(root).unwrap();
        let linker = Linker::new(prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        )
    }

    #[tokio::test]
    async fn upgrade_replaces_old_version_and_cleans_up() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle_v1 = create_bottle_tarball_with_version("testpkg", "1.0.0");
        let sha_v1 = sha256_hex(&bottle_v1);
        let bottle_v2 = create_bottle_tarball_with_version("testpkg", "2.0.0");
        let sha_v2 = sha256_hex(&bottle_v2);

        Mock::given(method("GET"))
            .and(path("/formula/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "testpkg",
                "1.0.0",
                tag,
                &sha_v1,
            )))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/testpkg-1.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle_v1))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "testpkg",
                "2.0.0",
                tag,
                &sha_v2,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/testpkg-2.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle_v2))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["testpkg".to_string()], true)
            .await
            .unwrap();
        assert!(root.join("cellar/testpkg/1.0.0").exists());
        assert!(prefix.join("bin/testpkg").exists());

        installer
            .upgrade("testpkg", false, true, None)
            .await
            .unwrap();

        assert!(root.join("cellar/testpkg/2.0.0").exists());
        assert!(
            !root.join("cellar/testpkg/1.0.0").exists(),
            "old cellar dir must be removed"
        );

        let bin_link = prefix.join("bin/testpkg");
        assert!(bin_link.exists(), "new version must be linked");
        let target = fs::read_link(&bin_link).unwrap();
        let target_str = target.to_string_lossy();
        assert!(
            target_str.contains("2.0.0"),
            "symlink must point at 2.0.0, got {target_str}"
        );
        assert!(
            !target_str.contains("1.0.0"),
            "symlink must not point at 1.0.0"
        );

        let installed = installer.get_installed("testpkg").unwrap();
        assert_eq!(installed.version, "2.0.0");
    }

    #[tokio::test]
    async fn plain_install_over_older_version_relinks_to_new_version() {
        // Regression test for #331 (https://github.com/zerobrewhq/zerobrew/issues/331):
        // `zb install <pkg>` with an older version already installed failed
        // the link step with conflicts "belonging to" the package itself,
        // leaving the DB reporting the new version while bin/<pkg> kept
        // resolving to the old keg.
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle_v1 = create_bottle_tarball_with_version("relinkpkg", "1.0.0");
        let sha_v1 = sha256_hex(&bottle_v1);
        let bottle_v2 = create_bottle_tarball_with_version("relinkpkg", "2.0.0");
        let sha_v2 = sha256_hex(&bottle_v2);

        Mock::given(method("GET"))
            .and(path("/formula/relinkpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "relinkpkg",
                "1.0.0",
                tag,
                &sha_v1,
            )))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/relinkpkg-1.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle_v1))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/relinkpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "relinkpkg",
                "2.0.0",
                tag,
                &sha_v2,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/relinkpkg-2.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle_v2))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["relinkpkg".to_string()], true)
            .await
            .unwrap();
        let bin_link = prefix.join("bin/relinkpkg");
        assert!(
            fs::read_link(&bin_link)
                .unwrap()
                .to_string_lossy()
                .contains("1.0.0")
        );

        installer
            .install(&["relinkpkg".to_string()], true)
            .await
            .expect("install over an older version must not fail the link step");

        let target = fs::read_link(&bin_link).unwrap();
        let target_str = target.to_string_lossy();
        assert!(
            target_str.contains("2.0.0"),
            "bin symlink must point at 2.0.0, got {target_str}"
        );
        assert!(bin_link.exists(), "bin symlink must not be dangling");

        let opt_target = fs::read_link(prefix.join("opt/relinkpkg")).unwrap();
        assert!(opt_target.to_string_lossy().contains("2.0.0"));

        let installed = installer.get_installed("relinkpkg").unwrap();
        assert_eq!(installed.version, "2.0.0");
    }

    #[tokio::test]
    async fn upgrade_with_no_link_does_not_create_symlinks() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle_v1 = create_bottle_tarball_with_version("nolinkpkg", "1.0.0");
        let sha_v1 = sha256_hex(&bottle_v1);
        let bottle_v2 = create_bottle_tarball_with_version("nolinkpkg", "2.0.0");
        let sha_v2 = sha256_hex(&bottle_v2);

        Mock::given(method("GET"))
            .and(path("/formula/nolinkpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "nolinkpkg",
                "1.0.0",
                tag,
                &sha_v1,
            )))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/nolinkpkg-1.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle_v1))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/nolinkpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "nolinkpkg",
                "2.0.0",
                tag,
                &sha_v2,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/nolinkpkg-2.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle_v2))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["nolinkpkg".to_string()], true)
            .await
            .unwrap();
        assert!(prefix.join("bin/nolinkpkg").exists());

        installer
            .upgrade("nolinkpkg", false, false, None)
            .await
            .unwrap();

        assert!(root.join("cellar/nolinkpkg/2.0.0").exists());
        assert!(!root.join("cellar/nolinkpkg/1.0.0").exists());
        assert!(
            !prefix.join("bin/nolinkpkg").exists(),
            "no symlinks expected when link=false"
        );
    }

    #[tokio::test]
    async fn upgrade_no_op_when_already_latest() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle = create_bottle_tarball("steadypkg");
        let sha = sha256_hex(&bottle);

        Mock::given(method("GET"))
            .and(path("/formula/steadypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "steadypkg",
                "1.0.0",
                tag,
                &sha,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/steadypkg-1.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["steadypkg".to_string()], true)
            .await
            .unwrap();

        installer
            .upgrade("steadypkg", false, true, None)
            .await
            .unwrap();

        assert!(root.join("cellar/steadypkg/1.0.0").exists());
        assert!(prefix.join("bin/steadypkg").exists());
        assert_eq!(
            installer.get_installed("steadypkg").unwrap().version,
            "1.0.0"
        );
    }

    #[tokio::test]
    async fn upgrade_errors_when_not_installed() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        let err = installer
            .upgrade("nonexistent", false, true, None)
            .await
            .unwrap_err();
        assert!(matches!(err, zb_core::Error::NotInstalled { .. }));
    }

    #[tokio::test]
    async fn upgrade_keeps_old_version_when_new_bottle_download_fails() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle = create_bottle_tarball("flakypkg");
        let sha = sha256_hex(&bottle);

        Mock::given(method("GET"))
            .and(path("/formula/flakypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "flakypkg",
                "1.0.0",
                tag,
                &sha,
            )))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/flakypkg-1.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Plan resolves to 2.0.0 with a valid sha, but the bottle download
        // returns 500 — exercise the pre-fetch failure path.
        Mock::given(method("GET"))
            .and(path("/formula/flakypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "flakypkg",
                "2.0.0",
                tag,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/flakypkg-2.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(500).set_body_string("download failed"))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["flakypkg".to_string()], true)
            .await
            .unwrap();
        assert!(root.join("cellar/flakypkg/1.0.0").exists());
        let bin_link = prefix.join("bin/flakypkg");
        assert!(bin_link.exists());

        let result = installer.upgrade("flakypkg", false, true, None).await;
        assert!(result.is_err(), "upgrade should fail when bottle 500s");

        assert!(
            root.join("cellar/flakypkg/1.0.0").exists(),
            "old cellar dir must be preserved on download failure"
        );
        assert!(
            !root.join("cellar/flakypkg/2.0.0").exists(),
            "no partial new cellar should exist"
        );
        assert!(bin_link.exists(), "symlink to old version must remain");
        let target = fs::read_link(&bin_link).unwrap();
        assert!(target.to_string_lossy().contains("1.0.0"));
        let installed = installer.get_installed("flakypkg").unwrap();
        assert_eq!(installed.version, "1.0.0");
    }
}
