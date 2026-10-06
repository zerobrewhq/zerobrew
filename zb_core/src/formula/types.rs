use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum KegOnly {
    #[default]
    No,
    Yes,
    Reason(String),
}

impl<'de> Deserialize<'de> for KegOnly {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Bool(true) => Ok(KegOnly::Yes),
            serde_json::Value::String(s) => Ok(KegOnly::Reason(s)),
            _ => Ok(KegOnly::No),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct KegOnlyReason {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub explanation: String,
}

impl KegOnlyReason {
    pub fn is_macos_specific(&self) -> bool {
        matches!(
            self.reason.as_str(),
            ":provided_by_macos" | ":shadowed_by_macos"
        )
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SourceUrl {
    pub url: String,
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct FormulaUrls {
    #[serde(default)]
    pub stable: Option<SourceUrl>,
    #[serde(default)]
    pub head: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RubySourceChecksum {
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsesFromMacos {
    Plain(String),
    WithContext { name: String, context: String },
}

impl<'de> Deserialize<'de> for UsesFromMacos {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(s) => Ok(UsesFromMacos::Plain(s)),
            serde_json::Value::Object(map) => {
                let (name, context) = map
                    .into_iter()
                    .next()
                    .ok_or_else(|| serde::de::Error::custom("empty uses_from_macos object"))?;
                let ctx = context.as_str().unwrap_or("runtime").to_string();
                Ok(UsesFromMacos::WithContext { name, context: ctx })
            }
            _ => Err(serde::de::Error::custom("unexpected uses_from_macos value")),
        }
    }
}

impl UsesFromMacos {
    pub fn name(&self) -> &str {
        match self {
            UsesFromMacos::Plain(name) => name,
            UsesFromMacos::WithContext { name, .. } => name,
        }
    }

    pub fn is_runtime_dependency(&self) -> bool {
        match self {
            UsesFromMacos::Plain(_) => true,
            UsesFromMacos::WithContext { context, .. } => context == "runtime",
        }
    }

    pub fn is_build_dependency(&self) -> bool {
        match self {
            UsesFromMacos::Plain(_) => false,
            UsesFromMacos::WithContext { context, .. } => context == "build",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Formula {
    pub name: String,
    pub versions: Versions,
    pub dependencies: Vec<String>,
    pub bottle: Bottle,
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub keg_only: KegOnly,
    #[serde(default)]
    pub keg_only_reason: Option<KegOnlyReason>,
    #[serde(default)]
    pub build_dependencies: Vec<String>,
    #[serde(default)]
    pub urls: Option<FormulaUrls>,
    #[serde(default)]
    pub ruby_source_path: Option<String>,
    #[serde(default)]
    pub ruby_source_checksum: Option<RubySourceChecksum>,
    #[serde(default)]
    pub uses_from_macos: Vec<UsesFromMacos>,
    #[serde(default)]
    pub requirements: Vec<serde_json::Value>,
    #[serde(default)]
    pub variations: Option<serde_json::Value>,
    /// Other names the formula goes by, like `pkg-config` for `pkgconf`.
    /// Homebrew links each one under `opt/` too.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Names the formula used to have, also linked under `opt/`.
    #[serde(default)]
    pub oldnames: Vec<String>,
}

impl Formula {
    pub fn effective_version(&self) -> String {
        if self.revision > 0 {
            format!("{}_{}", self.versions.stable, self.revision)
        } else {
            self.versions.stable.clone()
        }
    }

    pub fn is_keg_only(&self) -> bool {
        if matches!(self.keg_only, KegOnly::No) {
            return false;
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(ref reason) = self.keg_only_reason
            && reason.is_macos_specific()
        {
            return false;
        }
        true
    }

    pub fn source_url(&self) -> Option<&SourceUrl> {
        self.urls.as_ref().and_then(|u| u.stable.as_ref())
    }

    pub fn has_source_url(&self) -> bool {
        self.source_url().is_some()
    }

    pub fn all_build_dependencies(&self) -> Vec<String> {
        let deps = self.build_dependencies.clone();
        #[cfg(not(target_os = "macos"))]
        let deps = {
            let mut deps = deps;
            for u in self.active_uses_from_macos() {
                push_unique_dep(&mut deps, u.name());
            }
            deps
        };
        deps
    }

    pub fn runtime_dependencies(&self) -> Vec<String> {
        #[cfg(not(target_os = "macos"))]
        {
            let mut deps = self.platform_dependencies();
            for dep in self
                .active_uses_from_macos()
                .iter()
                .filter(|dep| dep.is_runtime_dependency())
            {
                push_unique_dep(&mut deps, dep.name());
            }
            deps
        }

        #[cfg(target_os = "macos")]
        {
            self.platform_dependencies()
        }
    }

    fn platform_dependencies(&self) -> Vec<String> {
        #[cfg(target_os = "linux")]
        if let Some(deps) = self.variation_dependencies(preferred_linux_variation_keys()) {
            return deps;
        }

        self.dependencies.clone()
    }

    #[cfg(target_os = "linux")]
    fn variation_dependencies(&self, keys: &[&str]) -> Option<Vec<String>> {
        let variations = self.variations.as_ref()?.as_object()?;
        for key in keys {
            if let Some(deps) = variations
                .get(*key)
                .and_then(|variation| variation.get("dependencies"))
                .and_then(|deps| deps.as_array())
            {
                return Some(
                    deps.iter()
                        .filter_map(|dep| dep.as_str().map(ToString::to_string))
                        .collect(),
                );
            }
        }
        None
    }

    #[cfg(not(target_os = "macos"))]
    fn active_uses_from_macos(&self) -> Vec<UsesFromMacos> {
        #[cfg(target_os = "linux")]
        if let Some(deps) = self.variation_uses_from_macos(preferred_linux_variation_keys()) {
            return deps;
        }

        self.uses_from_macos.clone()
    }

    #[cfg(not(target_os = "macos"))]
    fn variation_uses_from_macos(&self, keys: &[&str]) -> Option<Vec<UsesFromMacos>> {
        let variations = self.variations.as_ref()?.as_object()?;
        for key in keys {
            if let Some(value) = variations
                .get(*key)
                .and_then(|variation| variation.get("uses_from_macos"))
            {
                return serde_json::from_value(value.clone()).ok();
            }
        }
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn push_unique_dep(deps: &mut Vec<String>, name: &str) {
    if !deps.iter().any(|existing| existing == name) {
        deps.push(name.to_string());
    }
}

#[cfg(target_os = "linux")]
fn preferred_linux_variation_keys() -> &'static [&'static str] {
    match std::env::consts::ARCH {
        "aarch64" => &["arm64_linux", "aarch64_linux"],
        "x86_64" => &["x86_64_linux"],
        _ => &[],
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Versions {
    pub stable: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Bottle {
    pub stable: BottleStable,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BottleStable {
    pub files: BTreeMap<String, BottleFile>,
    /// Rebuild number for the bottle. When > 0, the bottle's internal paths
    /// use `{version}_{rebuild}` instead of just `{version}`.
    #[serde(default)]
    pub rebuild: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BottleFile {
    pub url: String,
    pub sha256: String,
    /// Where Homebrew expects the bottle to be poured: `:any`,
    /// `:any_skip_relocation`, or a fixed Cellar such as `/opt/homebrew/Cellar`.
    #[serde(default)]
    pub cellar: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_formula_fixtures() {
        let fixtures = [
            include_str!("../../fixtures/formula_foo.json"),
            include_str!("../../fixtures/formula_bar.json"),
        ];

        for fixture in fixtures {
            let formula: Formula = serde_json::from_str(fixture).unwrap();
            assert!(!formula.name.is_empty());
            assert!(!formula.versions.stable.is_empty());
            assert!(!formula.bottle.stable.files.is_empty());
        }
    }

    #[test]
    fn effective_version_without_revision() {
        let fixture = include_str!("../../fixtures/formula_foo.json");
        let formula: Formula = serde_json::from_str(fixture).unwrap();

        // Without revision, effective_version should equal stable version
        assert_eq!(formula.revision, 0);
        assert_eq!(formula.effective_version(), "1.2.3");
    }

    #[test]
    fn effective_version_with_revision() {
        // Manually construct formula with revision since we don't have a fixture for it yet
        let mut formula: Formula =
            serde_json::from_str(include_str!("../../fixtures/formula_foo.json")).unwrap();
        formula.revision = 1;

        // With revision=1, effective_version should be "1.2.3_1"
        assert_eq!(formula.effective_version(), "1.2.3_1");
    }

    #[test]
    fn effective_version_ignores_rebuild_for_dir_name() {
        let fixture = include_str!("../../fixtures/formula_with_rebuild.json");
        let formula: Formula = serde_json::from_str(fixture).unwrap();

        // With rebuild=1 but revision=0, effective_version should NOT have suffix
        assert_eq!(formula.bottle.stable.rebuild, 1);
        assert_eq!(formula.revision, 0);
        assert_eq!(formula.effective_version(), "8.0.1");
    }

    #[test]
    fn revision_field_defaults_to_zero() {
        let fixture = include_str!("../../fixtures/formula_foo.json");
        let formula: Formula = serde_json::from_str(fixture).unwrap();
        assert_eq!(formula.revision, 0);
    }

    #[test]
    fn keg_only_defaults_to_no() {
        let fixture = include_str!("../../fixtures/formula_foo.json");
        let formula: Formula = serde_json::from_str(fixture).unwrap();
        assert_eq!(formula.keg_only, KegOnly::No);
        assert!(!formula.is_keg_only());
    }

    #[test]
    fn keg_only_deserializes_bool_true() {
        let json = r#"{
            "name": "libfoo",
            "versions": { "stable": "1.0" },
            "dependencies": [],
            "keg_only": true,
            "bottle": { "stable": { "files": {
                "arm64_sonoma": { "url": "https://x.com/a.tar.gz", "sha256": "aa" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        assert_eq!(formula.keg_only, KegOnly::Yes);
        assert!(formula.is_keg_only());
    }

    #[test]
    fn keg_only_deserializes_string_reason() {
        let json = r#"{
            "name": "libpq",
            "versions": { "stable": "16.0" },
            "dependencies": [],
            "keg_only": "it conflicts with PostgreSQL",
            "bottle": { "stable": { "files": {
                "arm64_sonoma": { "url": "https://x.com/a.tar.gz", "sha256": "aa" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        assert!(
            matches!(formula.keg_only, KegOnly::Reason(ref s) if s == "it conflicts with PostgreSQL")
        );
        assert!(formula.is_keg_only());
    }

    #[test]
    fn keg_only_reason_deserializes_provided_by_macos() {
        let json = r#"{
            "name": "sqlite",
            "versions": { "stable": "3.51.2" },
            "dependencies": [],
            "keg_only": true,
            "keg_only_reason": { "reason": ":provided_by_macos", "explanation": "" },
            "bottle": { "stable": { "files": {
                "arm64_sonoma": { "url": "https://x.com/a.tar.gz", "sha256": "aa" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        assert_eq!(formula.keg_only, KegOnly::Yes);
        let reason = formula.keg_only_reason.as_ref().unwrap();
        assert_eq!(reason.reason, ":provided_by_macos");
        assert!(reason.is_macos_specific());
    }

    #[test]
    fn keg_only_reason_shadowed_by_macos_is_macos_specific() {
        let reason = KegOnlyReason {
            reason: ":shadowed_by_macos".to_string(),
            explanation: String::new(),
        };
        assert!(reason.is_macos_specific());
    }

    #[test]
    fn keg_only_reason_generic_is_not_macos_specific() {
        let reason = KegOnlyReason {
            reason: ":versioned_formula".to_string(),
            explanation: String::new(),
        };
        assert!(!reason.is_macos_specific());
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn provided_by_macos_not_keg_only_on_linux() {
        let json = r#"{
            "name": "sqlite",
            "versions": { "stable": "3.51.2" },
            "dependencies": [],
            "keg_only": true,
            "keg_only_reason": { "reason": ":provided_by_macos", "explanation": "" },
            "bottle": { "stable": { "files": {
                "x86_64_linux": { "url": "https://x.com/a.tar.gz", "sha256": "aa" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        assert!(!formula.is_keg_only());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn provided_by_macos_still_keg_only_on_macos() {
        let json = r#"{
            "name": "sqlite",
            "versions": { "stable": "3.51.2" },
            "dependencies": [],
            "keg_only": true,
            "keg_only_reason": { "reason": ":provided_by_macos", "explanation": "" },
            "bottle": { "stable": { "files": {
                "arm64_sonoma": { "url": "https://x.com/a.tar.gz", "sha256": "aa" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        assert!(formula.is_keg_only());
    }

    #[test]
    fn keg_only_without_macos_reason_stays_keg_only() {
        let json = r#"{
            "name": "libfoo",
            "versions": { "stable": "1.0" },
            "dependencies": [],
            "keg_only": true,
            "keg_only_reason": { "reason": ":versioned_formula", "explanation": "" },
            "bottle": { "stable": { "files": {
                "arm64_sonoma": { "url": "https://x.com/a.tar.gz", "sha256": "aa" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        assert!(formula.is_keg_only());
    }

    #[test]
    fn keg_only_true_without_reason_field_stays_keg_only() {
        let json = r#"{
            "name": "libfoo",
            "versions": { "stable": "1.0" },
            "dependencies": [],
            "keg_only": true,
            "bottle": { "stable": { "files": {
                "arm64_sonoma": { "url": "https://x.com/a.tar.gz", "sha256": "aa" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        assert!(formula.keg_only_reason.is_none());
        assert!(formula.is_keg_only());
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn runtime_dependencies_include_runtime_uses_from_macos_on_linux() {
        let mut formula: Formula =
            serde_json::from_str(include_str!("../../fixtures/formula_foo.json")).unwrap();
        formula.dependencies = vec!["openssl@3".to_string()];
        formula.uses_from_macos = vec![
            UsesFromMacos::Plain("expat".to_string()),
            UsesFromMacos::WithContext {
                name: "pkgconf".to_string(),
                context: "build".to_string(),
            },
        ];

        assert_eq!(
            formula.runtime_dependencies(),
            vec!["openssl@3".to_string(), "expat".to_string()]
        );
    }

    #[test]
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn runtime_dependencies_use_linux_variation_dependencies() {
        let mut formula: Formula =
            serde_json::from_str(include_str!("../../fixtures/formula_foo.json")).unwrap();
        formula.dependencies = vec!["openssl@3".to_string()];
        formula.variations = Some(serde_json::json!({
            "x86_64_linux": { "dependencies": ["openssl@3", "zlib-ng-compat"] },
            "arm64_linux": { "dependencies": ["openssl@3", "zlib-ng-compat"] }
        }));
        formula.uses_from_macos = vec![UsesFromMacos::Plain("expat".to_string())];

        assert_eq!(
            formula.runtime_dependencies(),
            vec![
                "openssl@3".to_string(),
                "zlib-ng-compat".to_string(),
                "expat".to_string()
            ]
        );
    }

    #[test]
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn runtime_dependencies_include_linux_variation_uses_from_macos() {
        let mut formula: Formula =
            serde_json::from_str(include_str!("../../fixtures/formula_foo.json")).unwrap();
        formula.dependencies = vec!["openssl@3".to_string()];
        formula.uses_from_macos = vec![UsesFromMacos::Plain("expat".to_string())];
        formula.variations = Some(serde_json::json!({
            "x86_64_linux": {
                "dependencies": ["openssl@3", "zlib-ng-compat"],
                "uses_from_macos": ["libffi", { "pkgconf": "build" }]
            },
            "arm64_linux": {
                "dependencies": ["openssl@3", "zlib-ng-compat"],
                "uses_from_macos": ["libffi", { "pkgconf": "build" }]
            }
        }));

        assert_eq!(
            formula.runtime_dependencies(),
            vec![
                "openssl@3".to_string(),
                "zlib-ng-compat".to_string(),
                "libffi".to_string()
            ]
        );
    }

    #[test]
    fn bottle_file_cellar_deserializes() {
        let json = r#"{
            "name": "foo",
            "versions": { "stable": "1.0" },
            "dependencies": [],
            "bottle": { "stable": { "files": {
                "arm64_tahoe": { "url": "https://x.com/a.tar.gz", "sha256": "aa", "cellar": "/opt/homebrew/Cellar" },
                "arm64_sequoia": { "url": "https://x.com/b.tar.gz", "sha256": "bb", "cellar": ":any_skip_relocation" },
                "arm64_sonoma": { "url": "https://x.com/c.tar.gz", "sha256": "cc" }
            }}}
        }"#;
        let formula: Formula = serde_json::from_str(json).unwrap();
        let files = &formula.bottle.stable.files;
        assert_eq!(
            files["arm64_tahoe"].cellar.as_deref(),
            Some("/opt/homebrew/Cellar")
        );
        assert_eq!(
            files["arm64_sequoia"].cellar.as_deref(),
            Some(":any_skip_relocation")
        );
        assert_eq!(files["arm64_sonoma"].cellar, None);
    }
}
