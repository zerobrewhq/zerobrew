use std::collections::{BTreeMap, HashMap, HashSet};

use tracing::warn;
use zb_core::{
    BuildPlan, Error, Formula, InstallMethod, SelectedBottle, formula_token, select_bottle,
};

use super::{InstallPlan, Installer, PlanFailure, PlannedInstall};

impl Installer {
    pub async fn plan(&self, names: &[String]) -> Result<InstallPlan, Error> {
        self.plan_with_options(names, false).await
    }

    pub async fn plan_with_options(
        &self,
        names: &[String],
        build_from_source: bool,
    ) -> Result<InstallPlan, Error> {
        let formulas = self.fetch_all_formulas(names, build_from_source).await?;
        let forced = forced_source_names(names, &formulas, build_from_source);
        let ordered = zb_core::resolve_closure(names, &formulas, |f| {
            self.builds_from_source(f, forced.contains(&f.name))
        })?;

        let roots = root_install_names(names, &formulas);
        let mut items = Vec::with_capacity(ordered.len());
        let mut planned = HashSet::new();
        for requested in ordered {
            let formula = formulas.get(&requested).cloned().unwrap();
            let install_name = install_name_for(&requested, &formula);
            if !planned.insert(install_name.clone()) {
                continue;
            }
            if !roots.contains(&install_name) && self.has_installed_keg(&install_name) {
                continue;
            }
            let force = forced.contains(&formula.name);
            items.push(self.plan_item(install_name, formula, force)?);
        }

        Ok(InstallPlan { items })
    }

    pub async fn plan_best_effort(
        &self,
        names: &[String],
        build_from_source: bool,
    ) -> (InstallPlan, Vec<PlanFailure>) {
        let (formulas, fetch_failures) = self
            .fetch_all_formulas_best_effort(names, build_from_source)
            .await;
        let mut items = Vec::new();
        let mut failures = Vec::new();
        let mut valid_roots = Vec::new();
        let mut seen_roots = HashSet::new();
        let forced = forced_source_names(names, &formulas, build_from_source);
        let is_build_from_source =
            |f: &Formula| self.builds_from_source(f, forced.contains(&f.name));

        for name in names {
            if !seen_roots.insert(name.clone()) {
                continue;
            }

            if let Some(error) = fetch_failures.get(name) {
                failures.push(PlanFailure {
                    name: name.clone(),
                    error: error.clone(),
                });
                continue;
            }

            if !formulas.contains_key(name) {
                failures.push(PlanFailure {
                    name: name.clone(),
                    error: Error::MissingFormula { name: name.clone() },
                });
                continue;
            }

            if let Some(failure) =
                root_dependency_failure(name, &formulas, &fetch_failures, is_build_from_source)
            {
                failures.push(failure);
                continue;
            }

            valid_roots.push(name.clone());
        }

        if !valid_roots.is_empty() {
            match zb_core::resolve_closure(&valid_roots, &formulas, is_build_from_source) {
                Ok(ordered) => {
                    let roots = root_install_names(&valid_roots, &formulas);
                    let mut planned = HashSet::new();
                    for requested in ordered {
                        let formula = formulas.get(&requested).cloned().unwrap();
                        let install_name = install_name_for(&requested, &formula);
                        if !planned.insert(install_name.clone()) {
                            continue;
                        }
                        if !roots.contains(&install_name) && self.has_installed_keg(&install_name) {
                            continue;
                        }
                        let force = forced.contains(&formula.name);
                        match self.plan_item(install_name.clone(), formula, force) {
                            Ok(item) => items.push(item),
                            Err(error) => failures.push(PlanFailure {
                                name: install_name,
                                error,
                            }),
                        }
                    }
                }
                Err(error) => {
                    failures.extend(valid_roots.into_iter().map(|name| PlanFailure {
                        name,
                        error: error.clone(),
                    }));
                }
            }
        }

        (InstallPlan { items }, failures)
    }

    /// Whether a package is recorded as installed and its keg is on disk, at
    /// any version.
    ///
    /// Dependencies that are already installed are left out of the plan:
    /// installing a package must never upgrade other packages as a side
    /// effect. Only the packages named on the command line are brought to
    /// their latest version. A record whose keg has gone missing doesn't
    /// count, so the dependency is installed again.
    fn has_installed_keg(&self, install_name: &str) -> bool {
        self.db.get_installed(install_name).is_some_and(|keg| {
            self.cellar
                .has_keg(formula_token(install_name), &keg.version)
        })
    }

    fn plan_item(
        &self,
        install_name: String,
        formula: Formula,
        build_from_source: bool,
    ) -> Result<PlannedInstall, Error> {
        let method = if build_from_source {
            match BuildPlan::from_formula(&formula, &self.prefix) {
                Some(plan) => InstallMethod::Source(plan),
                None => match self.select_pourable_bottle(&formula) {
                    Ok(bottle) => InstallMethod::Bottle(bottle),
                    Err(_) => {
                        return Err(Error::UnsupportedBottle {
                            name: formula.name.clone(),
                        });
                    }
                },
            }
        } else {
            match self.select_pourable_bottle(&formula) {
                Ok(bottle) => InstallMethod::Bottle(bottle),
                Err(_) => match BuildPlan::from_formula(&formula, &self.prefix) {
                    Some(plan) => InstallMethod::Source(plan),
                    None => {
                        return Err(Error::UnsupportedBottle {
                            name: formula.name.clone(),
                        });
                    }
                },
            }
        };

        Ok(PlannedInstall {
            install_name,
            formula,
            method,
        })
    }

    /// Select a bottle that can be poured into this prefix.
    ///
    /// On macOS, bottles pinned to a shorter Cellar than ours (Intel bottles
    /// built for `/usr/local`) have hardcoded paths that can't be rewritten,
    /// so they're treated as unavailable and the formula is built from source
    /// instead, as Homebrew does for a non-default prefix.
    fn select_pourable_bottle(&self, formula: &Formula) -> Result<SelectedBottle, Error> {
        let bottle = select_bottle(formula)?;
        if cfg!(target_os = "macos") && !bottle.is_pourable_into(&self.prefix) {
            warn!(
                formula = %formula.name,
                cellar = bottle.cellar.as_deref().unwrap_or_default(),
                prefix = %self.prefix.display(),
                "bottle can't be relocated to this prefix; it will be built from source if possible"
            );
            return Err(Error::UnsupportedBottle {
                name: formula.name.clone(),
            });
        }
        Ok(bottle)
    }

    async fn fetch_all_formulas_best_effort(
        &self,
        names: &[String],
        build_from_source: bool,
    ) -> (BTreeMap<String, Formula>, HashMap<String, Error>) {
        let mut formulas = BTreeMap::new();
        let mut failures = HashMap::new();
        let mut fetched: HashSet<String> = HashSet::new();
        let mut to_fetch: Vec<String> = names.to_vec();

        while !to_fetch.is_empty() {
            let batch: Vec<String> = to_fetch
                .drain(..)
                .filter(|n| !fetched.contains(n))
                .collect();

            if batch.is_empty() {
                break;
            }

            for n in &batch {
                fetched.insert(n.clone());
            }

            let futures: Vec<_> = batch
                .iter()
                .map(|n| self.api_client.get_formula(n))
                .collect();

            let results = futures::future::join_all(futures).await;

            for (i, result) in results.into_iter().enumerate() {
                let fetch_name = batch[i].clone();
                let formula = match result {
                    Ok(f) => f,
                    Err(error) => {
                        failures.insert(fetch_name, error);
                        continue;
                    }
                };

                if select_bottle(&formula).is_err() && !formula.has_source_url() {
                    warn!(
                        formula = %formula.name,
                        "skipping formula with no bottle or source available for this platform"
                    );
                    failures.insert(
                        fetch_name,
                        Error::UnsupportedBottle {
                            name: formula.name.clone(),
                        },
                    );
                    continue;
                }

                let force = build_from_source && names.contains(&fetch_name);
                let is_build_source = self.builds_from_source(&formula, force);
                for dep in formula.install_dependencies(is_build_source) {
                    if !fetched.contains(&dep)
                        && !to_fetch.contains(&dep)
                        && !failures.contains_key(&dep)
                    {
                        to_fetch.push(dep);
                    }
                }

                formulas.insert(fetch_name, formula);
            }
        }

        (formulas, failures)
    }

    async fn fetch_all_formulas(
        &self,
        names: &[String],
        build_from_source: bool,
    ) -> Result<BTreeMap<String, Formula>, Error> {
        use std::collections::HashSet;

        let mut formulas = BTreeMap::new();
        let mut fetched: HashSet<String> = HashSet::new();
        let mut to_fetch: Vec<String> = names.to_vec();

        while !to_fetch.is_empty() {
            let batch: Vec<String> = to_fetch
                .drain(..)
                .filter(|n| !fetched.contains(n))
                .collect();

            if batch.is_empty() {
                break;
            }

            for n in &batch {
                fetched.insert(n.clone());
            }

            let futures: Vec<_> = batch
                .iter()
                .map(|n| self.api_client.get_formula(n))
                .collect();

            let results = futures::future::join_all(futures).await;

            for (i, result) in results.into_iter().enumerate() {
                let formula = match result {
                    Ok(f) => f,
                    Err(e) => return Err(e),
                };

                if select_bottle(&formula).is_err() && !formula.has_source_url() {
                    warn!(
                        formula = %formula.name,
                        "skipping formula with no bottle or source available for this platform"
                    );
                    continue;
                }

                let force = build_from_source && names.contains(&batch[i]);
                let is_build_from_source = self.builds_from_source(&formula, force);
                for dep in formula.install_dependencies(is_build_from_source) {
                    if !fetched.contains(&dep) && !to_fetch.contains(&dep) {
                        to_fetch.push(dep);
                    }
                }

                formulas.insert(batch[i].clone(), formula);
            }
        }

        Ok(formulas)
    }

    fn builds_from_source(&self, formula: &Formula, build_from_source: bool) -> bool {
        let can_build = BuildPlan::from_formula(formula, &self.prefix).is_some();
        if build_from_source {
            can_build
        } else {
            self.select_pourable_bottle(formula).is_err() && can_build
        }
    }
}

/// The name to record an install under. A formula reached through an alias
/// or old name, like `zb install python` or node's `uses_from_macos "python"`,
/// is recorded under its own name (`python@3.14`) so the database matches its
/// keg in the Cellar. Tap references keep their full name.
fn install_name_for(requested: &str, formula: &Formula) -> String {
    if formula_token(requested) == formula.name {
        requested.to_string()
    } else {
        formula.name.clone()
    }
}

/// `--build-from-source` applies only to the requested formulas.
/// dependencies still prefer bottles and fall back to source when none pours.
fn forced_source_names(
    names: &[String],
    formulas: &BTreeMap<String, Formula>,
    build_from_source: bool,
) -> HashSet<String> {
    if !build_from_source {
        return HashSet::new();
    }
    names
        .iter()
        .filter_map(|name| formulas.get(name).map(|f| f.name.clone()))
        .collect()
}

/// The install names of the packages the caller asked for, as opposed to the
/// dependencies pulled in for them.
fn root_install_names(names: &[String], formulas: &BTreeMap<String, Formula>) -> HashSet<String> {
    names
        .iter()
        .filter_map(|name| {
            formulas
                .get(name)
                .map(|formula| install_name_for(name, formula))
        })
        .collect()
}

fn root_dependency_failure(
    root: &str,
    formulas: &BTreeMap<String, Formula>,
    fetch_failures: &HashMap<String, Error>,
    is_build_from_source: impl Fn(&Formula) -> bool,
) -> Option<PlanFailure> {
    let mut seen = HashSet::new();
    let mut stack = vec![root.to_string()];

    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }

        let Some(formula) = formulas.get(&name) else {
            continue;
        };

        for dep in formula.install_dependencies(is_build_from_source(formula)) {
            if let Some(error) = fetch_failures.get(&dep) {
                return Some(PlanFailure {
                    name: root.to_string(),
                    error: Error::ExecutionError {
                        message: format!("dependency '{dep}' could not be planned: {error}"),
                    },
                });
            }

            if formulas.contains_key(&dep) {
                stack.push(dep);
            }
        }
    }

    None
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
    use crate::{InstallPlan, Installer, Linker};

    #[tokio::test]
    async fn plans_tapped_formula_with_core_dependency() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let dep_bottle = create_bottle_tarball("go");
        let dep_sha = sha256_hex(&dep_bottle);
        let tag = get_test_bottle_tag();
        let dep_json = format!(
            r#"{{
                "name": "go",
                "versions": {{ "stable": "1.24.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/go-1.24.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            dep_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/go.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&dep_json))
            .mount(&mock_server)
            .await;

        let tap_formula_rb = format!(
            r#"
class Terraform < Formula
  version "1.10.0"
  depends_on "go"
  bottle do
    root_url "{}/ghcr/hashicorp/tap"
    sha256 cellar: :any_skip_relocation, {}: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#,
            mock_server.uri(),
            tag
        );

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        );
        let plan = installer
            .plan(&["hashicorp/tap/terraform".to_string()])
            .await
            .unwrap();

        let planned_names: Vec<String> = plan
            .items
            .iter()
            .map(|item| item.formula.name.clone())
            .collect();
        assert!(planned_names.contains(&"terraform".to_string()));
        assert!(planned_names.contains(&"go".to_string()));
    }

    #[tokio::test]
    async fn falls_back_to_source_when_no_bottle() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let formula_json = r#"{
            "name": "nobottle",
            "versions": { "stable": "1.0.0" },
            "dependencies": [],
            "build_dependencies": ["pkgconf"],
            "urls": {
                "stable": {
                    "url": "https://example.com/nobottle-1.0.0.tar.gz",
                    "checksum": "abc123"
                }
            },
            "ruby_source_path": "Formula/n/nobottle.rb",
            "bottle": { "stable": { "files": {} } }
        }"#;

        Mock::given(method("GET"))
            .and(path("/formula/nobottle.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/pkgconf.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(bottled_formula("pkgconf", &[])),
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let plan = installer.plan(&["nobottle".to_string()]).await.unwrap();

        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.items[0].formula.name, "pkgconf");
        assert!(matches!(
            plan.items[1].method,
            zb_core::InstallMethod::Source(_)
        ));

        if let zb_core::InstallMethod::Source(ref bp) = plan.items[0].method {
            assert_eq!(bp.source_url, "https://example.com/nobottle-1.0.0.tar.gz");
            assert_eq!(bp.formula_name, "nobottle");
            assert_eq!(bp.build_dependencies, vec!["pkgconf"]);
        }
    }

    /// An installer backed by a mock API serving each `(path name, formula
    /// JSON)`, like an alias whose JSON is the formula it points to.
    async fn installer_serving(
        mock_server: &MockServer,
        tmp: &TempDir,
        formulas: &[(&str, String)],
    ) -> Installer {
        for (name, json) in formulas {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{name}.json")))
                .respond_with(ResponseTemplate::new(200).set_body_string(json))
                .mount(mock_server)
                .await;
        }

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();
        Installer::new(
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            Store::new(&root).unwrap(),
            Cellar::new(&root).unwrap(),
            Linker::new(&prefix).unwrap(),
            Database::open(&root.join("db/zb.sqlite3")).unwrap(),
            prefix,
            root.join("locks"),
        )
    }

    fn bottled_formula(name: &str, dependencies: &[&str]) -> String {
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": {deps:?},
                "bottle": {{ "stable": {{ "files": {{
                    "{tag}": {{ "url": "https://example.com/{name}.tar.gz", "sha256": "aabbccdd" }}
                }} }} }}
            }}"#,
            deps = dependencies,
            tag = get_test_bottle_tag(),
        )
    }

    #[tokio::test]
    async fn aliases_are_planned_under_the_formula_name() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("node", bottled_formula("node", &["python"])),
                // The API resolves the `python` alias to python@3.14.
                ("python", bottled_formula("python@3.14", &[])),
            ],
        )
        .await;

        let plan = installer.plan(&["node".to_string()]).await.unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["python@3.14", "node"]);
    }

    #[tokio::test]
    async fn alias_and_formula_name_are_planned_once() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("python", bottled_formula("python@3.14", &[])),
                ("python@3.14", bottled_formula("python@3.14", &[])),
            ],
        )
        .await;

        let plan = installer
            .plan(&["python".to_string(), "python@3.14".to_string()])
            .await
            .unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["python@3.14"]);
    }

    /// Record `name` as installed at `version`, with its keg on disk.
    fn mark_installed(installer: &mut Installer, name: &str, version: &str) {
        fs::create_dir_all(installer.keg_path(name, version)).unwrap();
        let tx = installer.db.transaction().unwrap();
        tx.record_install(name, version, "oldsha").unwrap();
        tx.commit().unwrap();
    }

    #[tokio::test]
    async fn installed_dependencies_are_not_upgraded() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3", "libssh2"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
                ("libssh2", bottled_formula("libssh2", &["openssl@3"])),
            ],
        )
        .await;
        // An older openssl@3 is installed; the API now offers 1.0.0.
        mark_installed(&mut installer, "openssl@3", "0.9.0");

        let plan = installer.plan(&["nmap".to_string()]).await.unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["libssh2", "nmap"]);
    }

    #[tokio::test]
    async fn best_effort_plan_does_not_upgrade_installed_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
            ],
        )
        .await;
        mark_installed(&mut installer, "openssl@3", "0.9.0");

        let (plan, failures) = installer
            .plan_best_effort(&["nmap".to_string()], false)
            .await;

        assert!(failures.is_empty());
        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["nmap"]);
    }

    #[tokio::test]
    async fn explicitly_requested_installed_package_is_still_planned() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
            ],
        )
        .await;
        mark_installed(&mut installer, "openssl@3", "0.9.0");

        let plan = installer
            .plan(&["nmap".to_string(), "openssl@3".to_string()])
            .await
            .unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["openssl@3", "nmap"]);
    }

    #[tokio::test]
    async fn dependency_with_missing_keg_is_reinstalled() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("nmap", bottled_formula("nmap", &["openssl@3"])),
                ("openssl@3", bottled_formula("openssl@3", &[])),
            ],
        )
        .await;
        mark_installed(&mut installer, "openssl@3", "0.9.0");
        fs::remove_dir_all(installer.keg_path("openssl@3", "0.9.0")).unwrap();

        let plan = installer.plan(&["nmap".to_string()]).await.unwrap();

        let names: Vec<_> = plan.items.iter().map(|i| i.install_name.as_str()).collect();
        assert_eq!(names, ["openssl@3", "nmap"]);
    }

    #[tokio::test]
    async fn prefers_bottle_over_source() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "hasboth",
                "versions": {{ "stable": "2.0.0" }},
                "dependencies": [],
                "urls": {{
                    "stable": {{
                        "url": "https://example.com/hasboth-2.0.0.tar.gz",
                        "checksum": "def456"
                    }}
                }},
                "ruby_source_path": "Formula/h/hasboth.rb",
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "https://example.com/hasboth.bottle.tar.gz",
                                "sha256": "aabbccdd"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag
        );

        Mock::given(method("GET"))
            .and(path("/formula/hasboth.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let plan = installer.plan(&["hasboth".to_string()]).await.unwrap();

        assert_eq!(plan.items.len(), 1);
        assert!(matches!(
            plan.items[0].method,
            zb_core::InstallMethod::Bottle(_)
        ));
    }

    #[tokio::test]
    async fn errors_when_no_bottle_and_no_source() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let formula_json = r#"{
            "name": "nothing",
            "versions": { "stable": "1.0.0" },
            "dependencies": [],
            "bottle": { "stable": { "files": {} } }
        }"#;

        Mock::given(method("GET"))
            .and(path("/formula/nothing.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let result = installer.plan(&["nothing".to_string()]).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            zb_core::Error::MissingFormula { .. }
        ));
    }

    #[tokio::test]
    async fn plan_best_effort_keeps_valid_formula_when_another_is_missing() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "goodpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag
        );

        Mock::given(method("GET"))
            .and(path("/formula/goodpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/missingpkg.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let names = vec!["goodpkg".to_string(), "missingpkg".to_string()];
        let (plan, failures) = installer.plan_best_effort(&names, false).await;

        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].install_name, "goodpkg");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "missingpkg");
        assert!(matches!(
            failures[0].error,
            zb_core::Error::MissingFormula { .. }
        ));
    }

    fn buildable_formula(name: &str, cellar: Option<&str>) -> String {
        let cellar = cellar
            .map(|c| format!(r##", "cellar":"{c}""##))
            .unwrap_or_default();
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "build_dependencies": ["pkgconf"],
                "urls": {{ "stable": {{ "url": "https://example.com/{name}-1.0.0.tar.gz", "checksum": "abc123" }} }},
                "ruby_source_path": "Formula/{first}/{name}.rb",
                "bottle": {{ "stable": {{ "files": {{
                    "{tag}": {{ "url": "https://example.com/{name}.tar.gz", "sha256": "aabbccdd"{cellar} }}
                }} }} }}
            }}"#,
            first = &name[..1],
            tag = get_test_bottle_tag(),
        )
    }

    fn assert_source_with_build_dep(plan: &InstallPlan, name: &str) {
        let names: Vec<_> = plan.items.iter().map(|i| i.formula.name.as_str()).collect();
        assert!(
            names.contains(&"pkgconf"),
            "build dep missing from plan: {names:?}"
        );
        let item = plan.items.iter().find(|i| i.formula.name == name).unwrap();
        assert!(matches!(item.method, zb_core::InstallMethod::Source(_)));
    }

    // bottle exists,but user forces source build.
    #[tokio::test]
    async fn build_from_source_flag_plans_build_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("tool", buildable_formula("tool", None)),
                ("pkgconf", bottled_formula("pkgconf", &[])),
            ],
        )
        .await;

        let plan = installer
            .plan_with_options(&["tool".to_string()], true)
            .await
            .unwrap();
        assert_source_with_build_dep(&plan, "tool");
    }

    // The flag must not force dependencies (or their build deps) from source.
    #[tokio::test]
    async fn build_from_source_flag_only_applies_to_roots() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let mut app: serde_json::Value =
            serde_json::from_str(&buildable_formula("app", None)).unwrap();
        app["dependencies"] = serde_json::json!(["lib"]);
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("app", app.to_string()),
                ("lib", buildable_formula("lib", None)),
                ("pkgconf", bottled_formula("pkgconf", &[])),
            ],
        )
        .await;

        let plan = installer
            .plan_with_options(&["app".to_string()], true)
            .await
            .unwrap();
        assert_source_with_build_dep(&plan, "app");
        let lib = plan.items.iter().find(|i| i.formula.name == "lib").unwrap();
        assert!(matches!(lib.method, zb_core::InstallMethod::Bottle(_)));
    }

    // select_pourable_bottle rejects the bottle in macos.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unpourable_bottle_plans_build_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let installer = installer_serving(
            &mock_server,
            &tmp,
            &[
                ("tool", buildable_formula("tool", Some("/usr/local/Cellar"))),
                ("pkgconf", bottled_formula("pkgconf", &[])),
            ],
        )
        .await;

        let plan = installer.plan(&["tool".to_string()]).await.unwrap();
        assert_source_with_build_dep(&plan, "tool");
    }
}
