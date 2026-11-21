use std::fs;

use zb_core::{Error, Formula};

use super::Installer;
use crate::installer::post_install::{PostInstallContext, expand_caveats, run_steps};

use super::FormulaCaveats;

impl Installer {
    fn post_install_context(&self, formula: &Formula) -> PostInstallContext {
        PostInstallContext {
            prefix: self.prefix.clone(),
            cellar: self.cellar.dir().to_path_buf(),
            name: formula.name.clone(),
            version: formula.versions.stable.clone(),
            keg: self
                .cellar
                .keg_path(&formula.name, &formula.effective_version()),
            temp: self.prefix.join("tmp"),
            cache: self.cache_dir.join("post-install"),
        }
    }

    /// The formula's caveats with paths expanded for this prefix, if it has any.
    pub(super) fn formula_caveats(&self, formula: &Formula) -> Option<FormulaCaveats> {
        let text = formula.caveats.as_deref()?.trim_end();
        if text.is_empty() {
            return None;
        }
        Some(FormulaCaveats {
            name: formula.name.clone(),
            text: expand_caveats(&self.post_install_context(formula), text),
        })
    }

    /// Run a formula's post-install steps against its installed keg.
    /// Returns the messages of its `warn` steps.
    pub(super) async fn run_post_install(&self, formula: &Formula) -> Result<Vec<String>, Error> {
        if formula.post_install_steps.is_empty() {
            return Ok(Vec::new());
        }
        let ctx = self.post_install_context(formula);
        fs::create_dir_all(&ctx.temp).map_err(Error::file("failed to create tmp directory"))?;
        let steps = formula.post_install_steps.clone();
        let name = formula.name.clone();
        tokio::task::spawn_blocking(move || {
            run_steps(&ctx, &steps).map_err(|failure| Error::PostInstallFailed {
                name,
                step: failure.kind,
                message: failure.error.to_string(),
            })
        })
        .await
        .map_err(Error::exec("post-install task failed"))?
    }

    /// Run the post-install steps of an installed formula again, as
    /// `brew postinstall` does. The steps come from the current formula
    /// definition and run against the installed version.
    pub async fn post_install(&mut self, name: &str) -> Result<Vec<String>, Error> {
        let installed = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;
        let formula = self.installed_formula(name, &installed.version).await?;
        self.run_post_install(&formula).await
    }

    /// The caveats of an installed formula, as `zb info` shows them. They
    /// come from the current formula definition, like `brew info`.
    pub async fn installed_caveats(&self, name: &str) -> Result<Option<FormulaCaveats>, Error> {
        let installed = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;
        let formula = self.installed_formula(name, &installed.version).await?;
        Ok(self.formula_caveats(&formula))
    }

    /// The current formula definition, pointed at the keg that is installed.
    async fn installed_formula(&self, name: &str, version: &str) -> Result<Formula, Error> {
        let mut formula = self.api_client.get_formula(name).await?;
        if formula.effective_version() != version {
            let (stable, revision) = match version.rsplit_once('_') {
                Some((stable, revision)) if revision.chars().all(|c| c.is_ascii_digit()) => {
                    (stable.to_string(), revision.parse().unwrap_or(0))
                }
                _ => (version.to_string(), 0),
            };
            formula.versions.stable = stable;
            formula.revision = revision;
        }
        Ok(formula)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::test_support::formula_json_with;

    #[tokio::test]
    async fn post_install_reruns_steps_for_an_installed_formula() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut installer, _root, prefix) =
            super::super::tests::bare_installer(&mock_server, &tmp).await;

        let steps = r#"[{"type": "mkdir_p", "path": {"path": "{{HOMEBREW_PREFIX}}/var/made-by/{{formula_name}}-{{version}}"}}]"#;
        Mock::given(method("GET"))
            .and(path("/formula/thing.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(formula_json_with(
                    &mock_server.uri(),
                    "thing",
                    "2.0.0",
                    "deadbeef",
                    &format!(r#""post_install_steps": {steps}"#),
                )),
            )
            .mount(&mock_server)
            .await;
        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("thing", "2.0.0", "deadbeef").unwrap();
            tx.commit().unwrap();
        }
        fs::create_dir_all(installer.cellar.keg_path("thing", "2.0.0")).unwrap();

        let notices = installer.post_install("thing").await.unwrap();
        assert!(notices.is_empty());
        assert!(prefix.join("var/made-by/thing-2.0.0").is_dir());
    }

    #[tokio::test]
    async fn installed_caveats_come_from_the_formula_with_paths_expanded() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut installer, _root, prefix) =
            super::super::tests::bare_installer(&mock_server, &tmp).await;

        Mock::given(method("GET"))
            .and(path("/formula/thing.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(formula_json_with(
                    &mock_server.uri(),
                    "thing",
                    "2.0.0",
                    "deadbeef",
                    r#""caveats": "Config is in $HOMEBREW_PREFIX/etc/thing.conf\n""#,
                )),
            )
            .mount(&mock_server)
            .await;
        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("thing", "2.0.0", "deadbeef").unwrap();
            tx.commit().unwrap();
        }

        let caveats = installer.installed_caveats("thing").await.unwrap().unwrap();
        assert_eq!(caveats.name, "thing");
        assert_eq!(
            caveats.text,
            format!("Config is in {}/etc/thing.conf", prefix.display())
        );
        assert!(installer.installed_caveats("other").await.is_err());
    }

    #[tokio::test]
    async fn post_install_uses_the_installed_version_when_the_formula_moved_on() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut installer, _root, prefix) =
            super::super::tests::bare_installer(&mock_server, &tmp).await;

        let steps =
            r#"[{"type": "touch", "path": {"base": "prefix", "path": "touched-{{version}}"}}]"#;
        Mock::given(method("GET"))
            .and(path("/formula/thing.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(formula_json_with(
                    &mock_server.uri(),
                    "thing",
                    "3.0.0",
                    "cafe",
                    &format!(r#""post_install_steps": {steps}"#),
                )),
            )
            .mount(&mock_server)
            .await;
        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("thing", "2.0.0_1", "deadbeef").unwrap();
            tx.commit().unwrap();
        }
        let keg = installer.cellar.keg_path("thing", "2.0.0_1");
        fs::create_dir_all(&keg).unwrap();

        installer.post_install("thing").await.unwrap();
        assert!(keg.join("touched-2.0.0").is_file());
        assert!(!prefix.join("Cellar/thing/3.0.0").exists());
    }

    #[tokio::test]
    async fn post_install_requires_an_installed_formula() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut installer, _root, _prefix) =
            super::super::tests::bare_installer(&mock_server, &tmp).await;
        let err = installer.post_install("ghost").await.unwrap_err();
        assert!(matches!(err, zb_core::Error::NotInstalled { .. }));
    }
}
