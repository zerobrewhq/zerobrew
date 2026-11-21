use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use zb_core::Error;

/// Represents a Homebrew package that can be migrated
#[derive(Debug, Clone)]
pub struct HomebrewPackage {
    pub name: String,
    pub tap: String,
    pub is_cask: bool,
}

/// Result of collecting Homebrew packages for migration
pub struct HomebrewMigrationPackages {
    /// Formulas from homebrew/core that can be migrated
    pub formulas: Vec<HomebrewPackage>,
    /// Formulas from non-core taps that cannot be migrated
    pub non_core_formulas: Vec<HomebrewPackage>,
    /// Cask packages that cannot be migrated
    pub casks: Vec<HomebrewPackage>,
    /// Leaves `brew info` could not resolve, e.g. formulae removed upstream
    pub unresolved: Vec<String>,
}

/// Parse Homebrew formulas from JSON output of `brew info --json=v1 --installed`
pub fn parse_formulas_from_json(json: &serde_json::Value) -> Vec<HomebrewPackage> {
    let mut packages = Vec::new();

    if let Some(formulas) = json.as_array() {
        for formula in formulas {
            if let Some(name) = formula.get("name").and_then(|n| n.as_str()) {
                let tap = formula
                    .get("tap")
                    .and_then(|t| t.as_str())
                    .unwrap_or("homebrew/core")
                    .to_string();

                packages.push(HomebrewPackage {
                    name: name.to_string(),
                    tap,
                    is_cask: false,
                });
            }
        }
    }

    packages
}

/// Parse Homebrew leaves from plain text output of `brew leaves`
pub fn parse_leaves_from_plain_text(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Parse Homebrew casks from plain text output of `brew list --cask`
pub fn parse_casks_from_plain_text(output: &str) -> Vec<HomebrewPackage> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|name| HomebrewPackage {
            name: name.to_string(),
            tap: "homebrew/cask".to_string(),
            is_cask: true,
        })
        .collect()
}

/// Categorize Homebrew packages for migration
///
/// Returns a struct with separate lists for:
/// - Formulas from homebrew/core (migratable)
/// - Formulas from other taps (not migratable)
/// - Cask packages (not migratable)
pub fn categorize_packages(packages: Vec<HomebrewPackage>) -> HomebrewMigrationPackages {
    let mut formulas = Vec::new();
    let mut non_core_formulas = Vec::new();
    let mut casks = Vec::new();

    for pkg in packages {
        if pkg.is_cask {
            casks.push(pkg);
        } else if pkg.tap == "homebrew/core" {
            formulas.push(pkg);
        } else {
            non_core_formulas.push(pkg);
        }
    }

    HomebrewMigrationPackages {
        formulas,
        non_core_formulas,
        casks,
        unresolved: Vec::new(),
    }
}

fn brew_info(brew: &Path, names: &[String]) -> Result<Vec<HomebrewPackage>, Error> {
    let output = Command::new(brew)
        .args(["info", "--json=v1"])
        .args(names)
        .output()
        .map_err(Error::exec("failed to run 'brew info'"))?;

    if !output.status.success() {
        return Err((Error::exec("brew info failed"))(String::from_utf8_lossy(
            &output.stderr,
        )));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(Error::exec("failed to parse brew info JSON"))?;
    Ok(parse_formulas_from_json(&json))
}

/// Look up the leaves with `brew info`, returning the formulae it resolved and
/// the names it could not.
///
/// One unknown name fails the whole batch call (an installed formula that was
/// since removed from homebrew-core, say), so on failure each leaf is retried alone.
pub fn resolve_leaves(
    brew: &Path,
    leaves: &[String],
) -> Result<(Vec<HomebrewPackage>, Vec<String>), Error> {
    if leaves.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let batch_err = match brew_info(brew, leaves) {
        Ok(formulas) => return Ok((formulas, Vec::new())),
        Err(e) => e,
    };

    let mut formulas = Vec::new();
    let mut unresolved = Vec::new();
    for leaf in leaves {
        match brew_info(brew, std::slice::from_ref(leaf)) {
            Ok(found) => formulas.extend(found),
            Err(_) => unresolved.push(leaf.clone()),
        }
    }
    // Every leaf failing means brew itself is broken, not one stale formula.
    if formulas.is_empty() {
        return Err(batch_err);
    }
    Ok((formulas, unresolved))
}

/// Get all installed Homebrew packages, categorized for migration
///
/// Only formulas from `homebrew/core` can be migrated to zerobrew.
/// Formulas from other taps and all casks are collected separately.
/// Only leaves are migrated, as there's no use to reinstalling dependencies.
pub fn get_homebrew_packages(brew: &Path) -> Result<HomebrewMigrationPackages, Error> {
    let leaves_output = Command::new(brew)
        .args(["leaves"])
        .output()
        .map_err(Error::exec("failed to run 'brew leaves'"))?;

    if !leaves_output.status.success() {
        return Err((Error::exec("brew leaves failed"))(
            String::from_utf8_lossy(&leaves_output.stderr),
        ));
    }

    let leaves = parse_leaves_from_plain_text(&String::from_utf8_lossy(&leaves_output.stdout));

    let (formulas, unresolved) = resolve_leaves(brew, &leaves)?;

    let casks_output = Command::new(brew)
        .args(["list", "--cask"])
        .output()
        .map_err(Error::exec("failed to run 'brew list --cask'"))?;

    if !casks_output.status.success() {
        return Err((Error::exec("brew list --cask failed"))(
            String::from_utf8_lossy(&casks_output.stderr),
        ));
    }

    let casks = parse_casks_from_plain_text(&String::from_utf8_lossy(&casks_output.stdout));

    let all_packages: Vec<HomebrewPackage> = formulas.into_iter().chain(casks).collect();
    let mut packages = categorize_packages(all_packages);
    packages.unresolved = unresolved;
    Ok(packages)
}

/// Resolve and pin the PATH entry before migration can install or uninstall anything.
pub fn find_homebrew() -> Result<PathBuf, Error> {
    for dir in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        let path = dir.join("brew");
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        let path = fs::canonicalize(path).map_err(Error::file("cannot locate Homebrew"))?;
        validate_homebrew(&path)?;
        return Ok(path);
    }
    Err(Error::ExecutionError {
        message: "Homebrew's brew was not found on PATH; migration requires real Homebrew".into(),
    })
}

fn validate_homebrew(path: &Path) -> Result<(), Error> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(Error::exec("cannot check Homebrew version"))?;
    if !output.status.success() || !output.stdout.starts_with(b"Homebrew ") {
        return Err(Error::ExecutionError {
            message: format!(
                "{} is not real Homebrew; migration cannot use zerobrew's brew compatibility entrypoint. Remove it with `zb install-alias brew --remove` and put Homebrew on PATH",
                path.display()
            ),
        });
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn migration_rejects_compatibility_entrypoint_before_package_commands() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let brew = dir.path().join("brew");
        fs::write(&brew, "#!/bin/sh\n[ \"$1\" = --version ] || exit 99\nprintf 'zerobrew brew compatibility 0.4.0\\n'\n").unwrap();
        fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            validate_homebrew(&brew)
                .unwrap_err()
                .to_string()
                .contains("not real Homebrew")
        );
        fs::write(&brew, "#!/bin/sh\nprintf 'Homebrew 7.0.0\\n'\n").unwrap();
        validate_homebrew(&brew).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_leaves_skips_formulae_brew_info_cannot_find() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let brew = dir.path().join("brew");
        // Fake brew: any request naming python@3.8 fails, like a formula removed upstream.
        fs::write(
            &brew,
            r#"#!/bin/sh
shift 2
for n in "$@"; do
  if [ "$n" = "python@3.8" ]; then
    echo 'Error: No available formula with the name "python@3.8".' >&2
    exit 1
  fi
done
printf '['
sep=''
for n in "$@"; do printf '%s{"name":"%s","tap":"homebrew/core"}' "$sep" "$n"; sep=','; done
printf ']'
"#,
        )
        .unwrap();
        fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();

        let leaves = vec![
            "git".to_string(),
            "python@3.8".to_string(),
            "jq".to_string(),
        ];
        let (formulas, unresolved) = resolve_leaves(&brew, &leaves).unwrap();
        let names: Vec<_> = formulas.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["git", "jq"]);
        assert_eq!(unresolved, vec!["python@3.8"]);

        let (formulas, unresolved) = resolve_leaves(&brew, &leaves[..1]).unwrap();
        assert_eq!(formulas.len(), 1);
        assert!(unresolved.is_empty());

        let err = resolve_leaves(&brew, &leaves[1..2]).unwrap_err();
        assert!(err.to_string().contains("brew info failed"));
    }

    #[test]
    fn test_parse_formulas_from_json() {
        let brew_output = r#"[
            {
                "name": "git",
                "tap": "homebrew/core",
                "versions": { "stable": "2.40.0" }
            },
            {
                "name": "neovim",
                "tap": "homebrew/core",
                "versions": { "stable": "0.9.0" }
            }
        ]"#;

        let formulas_json: serde_json::Value = serde_json::from_str(brew_output).unwrap();
        let packages = parse_formulas_from_json(&formulas_json);

        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "git");
        assert_eq!(packages[0].tap, "homebrew/core");
        assert!(!packages[0].is_cask);
        assert_eq!(packages[1].name, "neovim");
        assert!(!packages[1].is_cask);
    }

    #[test]
    fn test_parse_formulas_handles_missing_tap() {
        let brew_output = r#"[
            {"name": "no-tap-formula"}
        ]"#;

        let formulas_json: serde_json::Value = serde_json::from_str(brew_output).unwrap();
        let packages = parse_formulas_from_json(&formulas_json);

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "no-tap-formula");
        assert_eq!(packages[0].tap, "homebrew/core");
    }

    #[test]
    fn test_parse_casks_from_plain_text() {
        // Simulate brew list --cask output
        let brew_output = "visual-studio-code\nfirefox\n";

        let packages = parse_casks_from_plain_text(brew_output);

        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "visual-studio-code");
        assert_eq!(packages[0].tap, "homebrew/cask");
        assert!(packages[0].is_cask);
        assert_eq!(packages[1].name, "firefox");
        assert!(packages[1].is_cask);
    }

    #[test]
    fn test_parse_leaves_from_plain_text() {
        let brew_output = "git\nneovim\nfirefox\n\n";

        let leaves = parse_leaves_from_plain_text(brew_output);

        assert_eq!(leaves, vec!["git", "neovim", "firefox"]);
    }

    #[test]
    fn test_parse_casks_handles_empty_output() {
        let brew_output = "";

        let packages = parse_casks_from_plain_text(brew_output);

        assert!(packages.is_empty());
    }

    #[test]
    fn test_parse_casks_handles_multiple_lines() {
        let brew_output = "visual-studio-code\nfirefox\ndocker\niterm2\n";

        let packages = parse_casks_from_plain_text(brew_output);

        assert_eq!(packages.len(), 4);
        assert_eq!(
            packages.iter().map(|p| &p.name).collect::<Vec<_>>(),
            vec!["visual-studio-code", "firefox", "docker", "iterm2"]
        );
    }

    #[test]
    fn test_categorize_packages_filters_core_formulas() {
        let packages = vec![
            HomebrewPackage {
                name: "git".to_string(),
                tap: "homebrew/core".to_string(),
                is_cask: false,
            },
            HomebrewPackage {
                name: "curl".to_string(),
                tap: "homebrew/core".to_string(),
                is_cask: false,
            },
        ];

        let result = categorize_packages(packages);

        assert_eq!(result.formulas.len(), 2);
        assert!(result.non_core_formulas.is_empty());
        assert!(result.casks.is_empty());
    }

    #[test]
    fn test_categorize_packages_filters_non_core_formulas() {
        let packages = vec![
            HomebrewPackage {
                name: "php".to_string(),
                tap: "shivammathur/php".to_string(),
                is_cask: false,
            },
            HomebrewPackage {
                name: "mysql".to_string(),
                tap: "homebrew/mysql".to_string(),
                is_cask: false,
            },
        ];

        let result = categorize_packages(packages);

        assert!(result.formulas.is_empty());
        assert_eq!(result.non_core_formulas.len(), 2);
        assert!(result.casks.is_empty());
    }

    #[test]
    fn test_categorize_packages_filters_casks() {
        let packages = vec![
            HomebrewPackage {
                name: "visual-studio-code".to_string(),
                tap: "homebrew/cask".to_string(),
                is_cask: true,
            },
            HomebrewPackage {
                name: "firefox".to_string(),
                tap: "homebrew/cask".to_string(),
                is_cask: true,
            },
        ];

        let result = categorize_packages(packages);

        assert!(result.formulas.is_empty());
        assert!(result.non_core_formulas.is_empty());
        assert_eq!(result.casks.len(), 2);
    }

    #[test]
    fn test_categorize_packages_mixed_packages() {
        let packages = vec![
            HomebrewPackage {
                name: "git".to_string(),
                tap: "homebrew/core".to_string(),
                is_cask: false,
            },
            HomebrewPackage {
                name: "php".to_string(),
                tap: "homebrew/php".to_string(),
                is_cask: false,
            },
            HomebrewPackage {
                name: "visual-studio-code".to_string(),
                tap: "homebrew/cask".to_string(),
                is_cask: true,
            },
        ];

        let result = categorize_packages(packages);

        assert_eq!(result.formulas.len(), 1);
        assert_eq!(result.formulas[0].name, "git");

        assert_eq!(result.non_core_formulas.len(), 1);
        assert_eq!(result.non_core_formulas[0].name, "php");

        assert_eq!(result.casks.len(), 1);
        assert_eq!(result.casks[0].name, "visual-studio-code");
    }

    #[test]
    fn test_homebrew_package_struct() {
        let pkg = HomebrewPackage {
            name: "test-formula".to_string(),
            tap: "homebrew/core".to_string(),
            is_cask: false,
        };

        assert_eq!(pkg.name, "test-formula");
        assert_eq!(pkg.tap, "homebrew/core");
        assert!(!pkg.is_cask);

        let cask = HomebrewPackage {
            name: "test-cask".to_string(),
            tap: "homebrew/cask".to_string(),
            is_cask: true,
        };

        assert!(cask.is_cask);
    }
}
