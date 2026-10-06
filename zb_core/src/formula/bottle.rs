use std::path::Path;

use crate::formula::types::BottleFile;
use crate::{Error, Formula};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedBottle {
    pub tag: String,
    pub url: String,
    pub sha256: String,
    /// The bottle's `cellar` value from the formula API; see [`BottleFile::cellar`].
    pub cellar: Option<String>,
}

impl SelectedBottle {
    fn from_file(tag: impl Into<String>, file: &BottleFile) -> Self {
        Self {
            tag: tag.into(),
            url: file.url.clone(),
            sha256: file.sha256.clone(),
            cellar: file.cellar.clone(),
        }
    }

    /// The Homebrew prefix the bottle was built in, i.e. the one its hardcoded
    /// paths point to. Relocatable bottles (`:any`, `:any_skip_relocation`)
    /// were built in the platform's default prefix.
    pub fn build_prefix(&self) -> &str {
        match self.pinned_cellar() {
            Some(cellar) => cellar.strip_suffix("/Cellar").unwrap_or(cellar),
            None => default_homebrew_prefix(),
        }
    }

    /// Whether the bottle can be poured into `prefix`. Bottles pinned to a
    /// Cellar keep hardcoded paths in their binaries, which can only be
    /// rewritten in place, so `prefix` can't be longer than the one they were
    /// built for. Homebrew builds these from source instead.
    pub fn is_pourable_into(&self, prefix: &Path) -> bool {
        match self.pinned_cellar() {
            Some(_) => prefix.as_os_str().len() <= self.build_prefix().len(),
            None => true,
        }
    }

    fn pinned_cellar(&self) -> Option<&str> {
        self.cellar
            .as_deref()
            .filter(|cellar| cellar.starts_with('/'))
    }
}

/// Where Homebrew installs by default on this platform, which is where its
/// bottles are built.
pub fn default_homebrew_prefix() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "/opt/homebrew"
    } else if cfg!(target_os = "macos") {
        "/usr/local"
    } else {
        "/home/linuxbrew/.linuxbrew"
    }
}

const MACOS_CODENAMES_NEWEST_FIRST: &[&str] = &["tahoe", "sequoia", "sonoma", "ventura"];

#[cfg(any(target_os = "linux", test))]
fn preferred_linux_bottle_tags_for_arch(arch: &str) -> &'static [&'static str] {
    match arch {
        "aarch64" => &["arm64_linux", "aarch64_linux"],
        "x86_64" => &["x86_64_linux"],
        _ => &[],
    }
}

#[cfg(target_os = "linux")]
fn preferred_linux_bottle_tags() -> &'static [&'static str] {
    preferred_linux_bottle_tags_for_arch(std::env::consts::ARCH)
}

#[cfg(any(target_os = "linux", test))]
fn is_compatible_linux_bottle_tag_for_arch(tag: &str, arch: &str) -> bool {
    preferred_linux_bottle_tags_for_arch(arch).contains(&tag)
}

#[cfg(target_os = "linux")]
fn is_compatible_linux_bottle_tag(tag: &str) -> bool {
    is_compatible_linux_bottle_tag_for_arch(tag, std::env::consts::ARCH)
}

#[cfg(target_os = "macos")]
pub fn macos_major_version() -> Option<u32> {
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    let version = String::from_utf8_lossy(&output.stdout);
    version.trim().split('.').next()?.parse().ok()
}

fn codename_for_major(major: u32) -> Option<&'static str> {
    match major {
        26 => Some("tahoe"),
        15 => Some("sequoia"),
        14 => Some("sonoma"),
        13 => Some("ventura"),
        _ => None,
    }
}

pub fn compatible_codenames(major_version: Option<u32>) -> Vec<&'static str> {
    let Some(major) = major_version else {
        return MACOS_CODENAMES_NEWEST_FIRST.to_vec();
    };

    let Some(pos) = codename_for_major(major)
        .and_then(|c| MACOS_CODENAMES_NEWEST_FIRST.iter().position(|&t| t == c))
    else {
        return MACOS_CODENAMES_NEWEST_FIRST.to_vec();
    };

    MACOS_CODENAMES_NEWEST_FIRST[pos..].to_vec()
}

pub fn select_bottle(formula: &Formula) -> Result<SelectedBottle, Error> {
    #[cfg(target_os = "macos")]
    let macos_version = macos_major_version();
    #[cfg(not(target_os = "macos"))]
    let macos_version: Option<u32> = None;

    select_bottle_with_version(formula, macos_version)
}

fn select_bottle_with_version(
    formula: &Formula,
    macos_version: Option<u32>,
) -> Result<SelectedBottle, Error> {
    // Consumed only in #[cfg(target_os = "macos")] blocks; silence unused-variable on Linux.
    let _ = &macos_version;

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let codenames = compatible_codenames(macos_version);
        let tags: Vec<String> = codenames.iter().map(|c| format!("arm64_{c}")).collect();

        for tag in &tags {
            if let Some(file) = formula.bottle.stable.files.get(tag.as_str()) {
                return Ok(SelectedBottle::from_file(tag.clone(), file));
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        let tags = compatible_codenames(macos_version);

        for tag in &tags {
            if let Some(file) = formula.bottle.stable.files.get(*tag) {
                return Ok(SelectedBottle::from_file(tag.to_string(), file));
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for &preferred_tag in preferred_linux_bottle_tags() {
            if let Some(file) = formula.bottle.stable.files.get(preferred_tag) {
                return Ok(SelectedBottle::from_file(preferred_tag.to_string(), file));
            }
        }
    }

    if let Some(file) = formula.bottle.stable.files.get("all") {
        return Ok(SelectedBottle::from_file("all".to_string(), file));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let codenames = compatible_codenames(macos_version);
        for (tag, file) in &formula.bottle.stable.files {
            if tag.starts_with("arm64_") && !tag.contains("linux") {
                let bare = tag.strip_prefix("arm64_").unwrap_or(tag);
                if codenames.contains(&bare) {
                    return Ok(SelectedBottle::from_file(tag.clone(), file));
                }
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        let codenames = compatible_codenames(macos_version);
        for (tag, file) in &formula.bottle.stable.files {
            if !tag.starts_with("arm64_") && !tag.contains("linux") && tag != "all" {
                if codenames.contains(&tag.as_str()) {
                    return Ok(SelectedBottle::from_file(tag.clone(), file));
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    for (tag, file) in &formula.bottle.stable.files {
        if is_compatible_linux_bottle_tag(tag) {
            return Ok(SelectedBottle::from_file(tag.clone(), file));
        }
    }

    Err(Error::UnsupportedBottle {
        name: formula.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::types::{Bottle, BottleFile, BottleStable, KegOnly, Versions};
    use std::collections::BTreeMap;

    #[test]
    fn selects_platform_bottle() {
        let fixture = include_str!("../../fixtures/formula_foo.json");
        let formula: Formula = serde_json::from_str(fixture).unwrap();

        let selected = select_bottle(&formula).unwrap();

        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            assert_eq!(selected.tag, "arm64_sonoma");
            assert_eq!(
                selected.url,
                "https://example.com/foo-1.2.3.arm64_sonoma.bottle.tar.gz"
            );
            assert_eq!(
                selected.sha256,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            );
        }

        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            assert_eq!(selected.tag, "sonoma");
            assert_eq!(
                selected.url,
                "https://example.com/foo-1.2.3.sonoma.bottle.tar.gz"
            );
            assert_eq!(
                selected.sha256,
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
            );
        }

        #[cfg(target_os = "linux")]
        {
            assert_eq!(selected.tag, "x86_64_linux");
            assert_eq!(
                selected.url,
                "https://example.com/foo-1.2.3.x86_64_linux.bottle.tar.gz"
            );
            assert_eq!(
                selected.sha256,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            );
        }
    }

    #[test]
    fn linux_arm_prefers_arm64_bottle_tags() {
        assert_eq!(
            preferred_linux_bottle_tags_for_arch("aarch64"),
            &["arm64_linux", "aarch64_linux"]
        );
    }

    #[test]
    fn linux_x86_64_prefers_x86_64_bottle_tags() {
        assert_eq!(
            preferred_linux_bottle_tags_for_arch("x86_64"),
            &["x86_64_linux"]
        );
    }

    #[test]
    fn linux_fallback_rejects_cross_arch_bottle_tags() {
        assert!(!is_compatible_linux_bottle_tag_for_arch(
            "x86_64_linux",
            "aarch64"
        ));
        assert!(!is_compatible_linux_bottle_tag_for_arch(
            "arm64_linux",
            "x86_64"
        ));
    }

    #[test]
    fn selects_all_bottle_for_universal_packages() {
        let mut files = BTreeMap::new();
        files.insert(
            "all".to_string(),
            BottleFile {
                url: "https://ghcr.io/v2/homebrew/core/ca-certificates/blobs/sha256:abc123"
                    .to_string(),
                sha256: "abc123".to_string(),
                cellar: None,
            },
        );

        let formula = Formula {
            name: "ca-certificates".to_string(),
            versions: Versions {
                stable: "2024-01-01".to_string(),
            },
            dependencies: Vec::new(),
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: Vec::new(),
            urls: None,
            ruby_source_path: None,
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
            aliases: Vec::new(),
            oldnames: Vec::new(),
        };

        let selected = select_bottle(&formula).unwrap();
        assert_eq!(selected.tag, "all");
        assert!(selected.url.contains("ca-certificates"));
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn errors_when_no_arm64_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "sonoma".to_string(),
            BottleFile {
                url: "https://example.com/legacy.tar.gz".to_string(),
                sha256: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                    .to_string(),
                cellar: None,
            },
        );

        let formula = Formula {
            name: "legacy".to_string(),
            versions: Versions {
                stable: "0.1.0".to_string(),
            },
            dependencies: Vec::new(),
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: Vec::new(),
            urls: None,
            ruby_source_path: None,
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
            aliases: Vec::new(),
            oldnames: Vec::new(),
        };

        let err = select_bottle(&formula).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedBottle { name } if name == "legacy"
        ));
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    fn errors_when_no_x86_64_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_sonoma".to_string(),
            BottleFile {
                url: "https://example.com/legacy.tar.gz".to_string(),
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
                cellar: None,
            },
        );

        let formula = Formula {
            name: "legacy".to_string(),
            versions: Versions {
                stable: "0.1.0".to_string(),
            },
            dependencies: Vec::new(),
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: Vec::new(),
            urls: None,
            ruby_source_path: None,
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
            aliases: Vec::new(),
            oldnames: Vec::new(),
        };

        let err = select_bottle(&formula).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedBottle { name } if name == "legacy"
        ));
    }

    #[test]
    fn compatible_codenames_on_sequoia_excludes_tahoe() {
        let codenames = compatible_codenames(Some(15));
        assert_eq!(codenames, vec!["sequoia", "sonoma", "ventura"]);
    }

    #[test]
    fn compatible_codenames_on_tahoe_includes_all() {
        let codenames = compatible_codenames(Some(26));
        assert_eq!(codenames, vec!["tahoe", "sequoia", "sonoma", "ventura"]);
    }

    #[test]
    fn compatible_codenames_on_sonoma_excludes_newer() {
        let codenames = compatible_codenames(Some(14));
        assert_eq!(codenames, vec!["sonoma", "ventura"]);
    }

    #[test]
    fn compatible_codenames_on_ventura_returns_only_ventura() {
        let codenames = compatible_codenames(Some(13));
        assert_eq!(codenames, vec!["ventura"]);
    }

    #[test]
    fn compatible_codenames_unknown_version_returns_all() {
        let codenames = compatible_codenames(Some(99));
        assert_eq!(codenames, vec!["tahoe", "sequoia", "sonoma", "ventura"]);
    }

    #[test]
    fn compatible_codenames_none_returns_all() {
        let codenames = compatible_codenames(None);
        assert_eq!(codenames, vec!["tahoe", "sequoia", "sonoma", "ventura"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn sequoia_user_skips_tahoe_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_tahoe".to_string(),
            BottleFile {
                url: "https://example.com/tahoe.tar.gz".to_string(),
                sha256: "aaaa".repeat(16),
                cellar: None,
            },
        );
        files.insert(
            "arm64_sequoia".to_string(),
            BottleFile {
                url: "https://example.com/sequoia.tar.gz".to_string(),
                sha256: "bbbb".repeat(16),
                cellar: None,
            },
        );

        let formula = Formula {
            name: "libpq".to_string(),
            versions: Versions {
                stable: "18.3".to_string(),
            },
            dependencies: Vec::new(),
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: Vec::new(),
            urls: None,
            ruby_source_path: None,
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
            aliases: Vec::new(),
            oldnames: Vec::new(),
        };

        let selected = select_bottle_with_version(&formula, Some(15)).unwrap();

        #[cfg(target_arch = "aarch64")]
        assert_eq!(selected.tag, "arm64_sequoia");

        #[cfg(target_arch = "x86_64")]
        assert_eq!(selected.tag, "all");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn tahoe_user_picks_tahoe_bottle() {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_tahoe".to_string(),
            BottleFile {
                url: "https://example.com/tahoe.tar.gz".to_string(),
                sha256: "aaaa".repeat(16),
                cellar: None,
            },
        );
        files.insert(
            "arm64_sequoia".to_string(),
            BottleFile {
                url: "https://example.com/sequoia.tar.gz".to_string(),
                sha256: "bbbb".repeat(16),
                cellar: None,
            },
        );

        let formula = Formula {
            name: "libpq".to_string(),
            versions: Versions {
                stable: "18.3".to_string(),
            },
            dependencies: Vec::new(),
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: Vec::new(),
            urls: None,
            ruby_source_path: None,
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
            aliases: Vec::new(),
            oldnames: Vec::new(),
        };

        let selected = select_bottle_with_version(&formula, Some(26)).unwrap();

        #[cfg(target_arch = "aarch64")]
        assert_eq!(selected.tag, "arm64_tahoe");

        #[cfg(target_arch = "x86_64")]
        assert_eq!(selected.tag, "all");
    }

    fn bottle_with_cellar(cellar: Option<&str>) -> SelectedBottle {
        SelectedBottle {
            tag: "arm64_tahoe".to_string(),
            url: "https://example.com/foo.tar.gz".to_string(),
            sha256: "aaaa".repeat(16),
            cellar: cellar.map(str::to_string),
        }
    }

    #[test]
    fn pinned_bottle_build_prefix_comes_from_its_cellar() {
        assert_eq!(
            bottle_with_cellar(Some("/opt/homebrew/Cellar")).build_prefix(),
            "/opt/homebrew"
        );
        assert_eq!(
            bottle_with_cellar(Some("/usr/local/Cellar")).build_prefix(),
            "/usr/local"
        );
    }

    #[test]
    fn relocatable_bottle_build_prefix_is_the_platform_default() {
        for cellar in [Some(":any"), Some(":any_skip_relocation"), None] {
            assert_eq!(
                bottle_with_cellar(cellar).build_prefix(),
                default_homebrew_prefix()
            );
        }
    }

    #[test]
    fn relocatable_bottles_pour_into_any_prefix() {
        let long_prefix = Path::new("/a/much/longer/prefix/than/homebrew/uses");
        for cellar in [Some(":any"), Some(":any_skip_relocation"), None] {
            assert!(bottle_with_cellar(cellar).is_pourable_into(long_prefix));
        }
    }

    #[test]
    fn pinned_bottles_only_pour_into_prefixes_that_fit() {
        let arm = bottle_with_cellar(Some("/opt/homebrew/Cellar"));
        assert!(arm.is_pourable_into(Path::new("/opt/zerobrew")));
        assert!(arm.is_pourable_into(Path::new("/opt/zb")));
        assert!(!arm.is_pourable_into(Path::new("/opt/zerobrew/prefix")));

        let intel = bottle_with_cellar(Some("/usr/local/Cellar"));
        assert!(!intel.is_pourable_into(Path::new("/opt/zerobrew")));
        assert!(intel.is_pourable_into(Path::new("/usr/local")));
    }
}
