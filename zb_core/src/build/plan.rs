use std::path::{Path, PathBuf};

use crate::Formula;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildSystem {
    Autoconf,
    Cmake,
    Meson,
    Make,
    RubyFormula,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    Bottle(crate::SelectedBottle),
    Source(BuildPlan),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    pub formula_name: String,
    pub version: String,
    pub source_url: String,
    pub source_checksum: Option<String>,
    pub ruby_source_path: Option<String>,
    pub build_dependencies: Vec<String>,
    pub runtime_dependencies: Vec<String>,
    pub detected_system: BuildSystem,
    pub prefix: PathBuf,
    pub cellar_path: PathBuf,
}

impl BuildPlan {
    pub fn from_formula(formula: &Formula, prefix: &Path) -> Option<Self> {
        let source = formula.source_url()?;
        let version = formula.effective_version();
        let cellar_path = prefix.join("Cellar").join(&formula.name).join(&version);

        let all_build_deps = formula.all_build_dependencies();
        let detected_system = detect_build_system(&source.url, &all_build_deps);

        Some(Self {
            formula_name: formula.name.clone(),
            version,
            source_url: source.url.clone(),
            source_checksum: source.checksum.clone(),
            ruby_source_path: formula.ruby_source_path.clone(),
            build_dependencies: all_build_deps,
            runtime_dependencies: formula.runtime_dependencies(),
            detected_system,
            prefix: prefix.to_path_buf(),
            cellar_path,
        })
    }
}

fn detect_build_system(source_url: &str, build_deps: &[String]) -> BuildSystem {
    let has_dep = |name: &str| build_deps.iter().any(|d| d == name);

    if has_dep("cmake") {
        return BuildSystem::Cmake;
    }
    if has_dep("meson") {
        return BuildSystem::Meson;
    }
    if source_url.ends_with(".tar.gz")
        || source_url.ends_with(".tar.xz")
        || source_url.ends_with(".tar.bz2")
    {
        return BuildSystem::Autoconf;
    }
    BuildSystem::RubyFormula
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::types::*;
    use std::collections::BTreeMap;

    fn test_formula(name: &str, source_url: &str, build_deps: &[&str]) -> Formula {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_sonoma".to_string(),
            BottleFile {
                url: format!("https://example.com/{name}.tar.gz"),
                sha256: "deadbeef".repeat(8),
                cellar: None,
            },
        );

        Formula {
            name: name.to_string(),
            versions: Versions {
                stable: "1.0.0".to_string(),
            },
            dependencies: vec!["libfoo".to_string()],
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: build_deps.iter().map(|s| s.to_string()).collect(),
            urls: Some(FormulaUrls {
                stable: Some(SourceUrl {
                    url: source_url.to_string(),
                    checksum: Some("abc123".to_string()),
                    tag: None,
                    revision: None,
                }),
                head: None,
            }),
            ruby_source_path: Some(format!("Formula/{}/{name}.rb", &name[..1])),
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
            aliases: Vec::new(),
            oldnames: Vec::new(),
        }
    }

    #[test]
    fn detects_cmake_from_build_deps() {
        let f = test_formula("libheif", "https://example.com/src.tar.gz", &["cmake"]);
        let prefix = PathBuf::from("/opt/zerobrew");
        let plan = BuildPlan::from_formula(&f, &prefix).unwrap();
        assert_eq!(plan.detected_system, BuildSystem::Cmake);
    }

    #[test]
    fn detects_meson_from_build_deps() {
        let f = test_formula("glib", "https://example.com/src.tar.xz", &["meson"]);
        let prefix = PathBuf::from("/opt/zerobrew");
        let plan = BuildPlan::from_formula(&f, &prefix).unwrap();
        assert_eq!(plan.detected_system, BuildSystem::Meson);
    }

    #[test]
    fn detects_autoconf_from_tarball_url() {
        let f = test_formula("wget", "https://ftp.gnu.org/wget-1.25.tar.gz", &["pkgconf"]);
        let prefix = PathBuf::from("/opt/zerobrew");
        let plan = BuildPlan::from_formula(&f, &prefix).unwrap();
        assert_eq!(plan.detected_system, BuildSystem::Autoconf);
    }

    #[test]
    fn returns_none_without_source_url() {
        let mut f = test_formula("wget", "https://example.com/src.tar.gz", &[]);
        f.urls = None;
        let prefix = PathBuf::from("/opt/zerobrew");
        assert!(BuildPlan::from_formula(&f, &prefix).is_none());
    }

    #[test]
    fn cellar_path_includes_version() {
        let f = test_formula("wget", "https://example.com/src.tar.gz", &[]);
        let prefix = PathBuf::from("/opt/zerobrew");
        let plan = BuildPlan::from_formula(&f, &prefix).unwrap();
        assert_eq!(
            plan.cellar_path,
            PathBuf::from("/opt/zerobrew/Cellar/wget/1.0.0")
        );
    }
}
