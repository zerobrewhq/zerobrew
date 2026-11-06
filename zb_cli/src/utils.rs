use console::style;
use std::path::{Path, PathBuf};
use zb_io::Installer;

pub fn normalize_formula_name(name: &str) -> Result<String, zb_core::Error> {
    let trimmed = name.trim();
    if let Some(token) = trimmed.strip_prefix("cask:") {
        if token.is_empty() {
            return Err(zb_core::Error::InvalidArgument {
                message: "cask token cannot be empty".to_string(),
            });
        }
        return Ok(trimmed.to_string());
    }

    if let Some((tap, formula)) = trimmed.rsplit_once('/') {
        if formula.is_empty() {
            return Err(zb_core::Error::MissingFormula {
                name: trimmed.to_string(),
            });
        }

        if tap == "homebrew/core" {
            return Ok(formula.to_string());
        }

        if tap == "homebrew/cask" {
            return Ok(format!("cask:{formula}"));
        }

        return Ok(trimmed.to_string());
    }

    Ok(trimmed.to_string())
}

pub fn format_formula_suggestions(requested: &str, suggestions: &[String]) -> Option<String> {
    if suggestions.is_empty() {
        return None;
    }

    let mut rendered = format!(
        "{} Formula '{}' was not found. Did you mean:\n",
        style("Hint:").cyan().bold(),
        style(requested).bold()
    );

    for suggestion in suggestions {
        rendered.push_str(&format!("      {}\n", style(suggestion).green()));
    }

    if let Some(top_suggestion) = suggestions.first() {
        rendered.push_str("\n      Try installing the closest match with zerobrew:\n");
        rendered.push_str(&format!(
            "      {}\n",
            style(format!("zb install {top_suggestion}")).cyan()
        ));
    }

    Some(rendered)
}

pub fn suggest_formula_matches(requested: &str, suggestions: &[String]) {
    if let Some(message) = format_formula_suggestions(requested, suggestions) {
        eprintln!();
        eprint!("{message}");
        eprintln!();
    }
}

pub async fn suggest_missing_formula_matches(
    installer: &Installer,
    error: &zb_core::Error,
) -> bool {
    if let zb_core::Error::MissingFormula { name } = error {
        if let Ok(suggestions) = installer.suggest_formulas(name, 3).await {
            suggest_formula_matches(name, &suggestions);
        }
        return true;
    }

    false
}

/// Pure formatter for the Termux fallback message, kept testable without
/// spawning `pkg`.
pub fn format_termux_fallback(formula: &str, pkg_available: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "      {} {}",
        style(formula).yellow().bold(),
        style("is not compatible with Termux - homebrew bottles are not available for Android.")
            .red()
            .bold()
    ));
    out.push('\n');
    out.push_str(&format!(
        "      {}",
        style("and cannot be installed on it.").red().bold()
    ));
    if pkg_available {
        out.push('\n');
        out.push_str(&format!(
            "      {}",
            style(format!("Try: pkg install {}", formula)).cyan()
        ));
    }
    out
}

/// Whether Termux's `pkg` can install `formula` (checked via `pkg show`).
pub fn pkg_show_succeeds(formula: &str) -> bool {
    std::process::Command::new("pkg")
        .arg("show")
        .arg(formula)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn suggest_homebrew(formula: &str, error: &zb_core::Error) {
    eprintln!();
    eprintln!(
        "{} This package can't be installed with zerobrew.",
        style("Note:").yellow().bold()
    );
    eprintln!("      Error: {}", error);
    eprintln!();

    // Error for Termux on android since homebrew
    // doesn't support bottles for this platform
    // details: https://github.com/zerobrewhq/zerobrew/pull/136
    if cfg!(target_os = "android") {
        let pkg_available = pkg_show_succeeds(formula);
        for line in format_termux_fallback(formula, pkg_available).lines() {
            eprintln!("{}", line);
        }
    } else {
        eprintln!("      Try installing with Homebrew instead:");
        eprintln!(
            "      {}",
            style(format!("brew install {}", formula)).cyan()
        );
    }

    eprintln!();
}

pub fn get_root_path(cli_root: Option<PathBuf>) -> PathBuf {
    if let Some(root) = cli_root {
        return root;
    }

    if let Ok(env_root) = std::env::var("ZEROBREW_ROOT") {
        return PathBuf::from(env_root);
    }

    let legacy_root = PathBuf::from("/opt/zerobrew");
    if legacy_root.exists() {
        return legacy_root;
    }

    if cfg!(target_os = "macos") {
        legacy_root
    } else {
        let xdg_data_home = std::env::var("XDG_DATA_HOME")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".local").join("share"))
                    .unwrap_or_else(|_| legacy_root.clone())
            });
        xdg_data_home.join("zerobrew")
    }
}

pub fn get_prefix_path(cli_prefix: Option<PathBuf>, root: &Path) -> PathBuf {
    if let Some(prefix) = cli_prefix {
        return prefix;
    }

    let env_prefix = std::env::var_os("ZEROBREW_PREFIX").map(PathBuf::from);
    get_prefix_path_for_os(env_prefix, root, cfg!(target_os = "macos"))
}

fn get_prefix_path_for_os(env_prefix: Option<PathBuf>, root: &Path, is_macos: bool) -> PathBuf {
    if let Some(prefix) = env_prefix
        && !(is_macos && is_legacy_macos_default_prefix(&prefix, root))
    {
        return prefix;
    }

    default_prefix_for_os(root, is_macos)
}

fn default_prefix_for_os(root: &Path, is_macos: bool) -> PathBuf {
    if is_macos {
        root.to_path_buf()
    } else {
        root.join("prefix")
    }
}

fn is_legacy_macos_default_prefix(prefix: &Path, root: &Path) -> bool {
    prefix == root.join("prefix")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zb_io::cellar::Cellar;
    use zb_io::network::ApiClient;
    use zb_io::storage::{BlobCache, Database, Store};
    use zb_io::{Installer, Linker};

    use super::{
        format_formula_suggestions, format_termux_fallback, get_prefix_path_for_os,
        normalize_formula_name, suggest_missing_formula_matches,
    };

    #[test]
    fn macos_default_prefix_is_root() {
        let root = PathBuf::from("/opt/zerobrew");

        assert_eq!(get_prefix_path_for_os(None, &root, true), root);
    }

    #[test]
    fn linux_default_prefix_is_root_prefix() {
        let root = PathBuf::from("/home/user/.local/share/zerobrew");

        assert_eq!(
            get_prefix_path_for_os(None, &root, false),
            root.join("prefix")
        );
    }

    #[test]
    fn macos_ignores_legacy_root_prefix_env_default() {
        let root = PathBuf::from("/opt/zerobrew");

        assert_eq!(
            get_prefix_path_for_os(Some(root.join("prefix")), &root, true),
            root
        );
    }

    #[test]
    fn macos_keeps_custom_env_prefix() {
        let root = PathBuf::from("/opt/zerobrew");
        let custom = PathBuf::from("/zb");

        assert_eq!(
            get_prefix_path_for_os(Some(custom.clone()), &root, true),
            custom
        );
    }

    #[test]
    fn termux_fallback_suggests_pkg_when_available() {
        let msg = format_termux_fallback("jq", true);
        assert!(msg.contains("pkg install jq"));
        assert!(!msg.contains("brew install"));
    }

    #[test]
    fn termux_fallback_omits_suggestion_when_unavailable() {
        let msg = format_termux_fallback("jq", false);
        assert!(!msg.contains("pkg install"));
        assert!(!msg.contains("brew install"));
    }

    #[test]
    fn linux_keeps_env_prefix() {
        let root = PathBuf::from("/home/user/.local/share/zerobrew");
        let env_prefix = PathBuf::from("/tmp/zb-prefix");

        assert_eq!(
            get_prefix_path_for_os(Some(env_prefix.clone()), &root, false),
            env_prefix
        );
    }

    #[test]
    fn normalize_core_tap_formula() {
        assert_eq!(
            normalize_formula_name("homebrew/core/wget").unwrap(),
            "wget".to_string()
        );
    }

    #[test]
    fn normalize_external_tap_formula_keeps_full_name() {
        assert_eq!(
            normalize_formula_name("hashicorp/tap/terraform").unwrap(),
            "hashicorp/tap/terraform".to_string()
        );
    }

    #[test]
    fn normalize_homebrew_cask_prefixes_token() {
        assert_eq!(
            normalize_formula_name("homebrew/cask/docker-desktop").unwrap(),
            "cask:docker-desktop".to_string()
        );
    }

    #[test]
    fn format_formula_suggestions_renders_list() {
        let rendered =
            format_formula_suggestions("pythn", &["python".to_string(), "pytest".to_string()])
                .unwrap();

        assert!(rendered.contains("Did you mean"));
        assert!(rendered.contains("python"));
        assert!(rendered.contains("pytest"));
        assert!(rendered.contains("zb install python"));
    }

    #[test]
    fn format_formula_suggestions_returns_none_for_empty_input() {
        assert!(format_formula_suggestions("pythn", &[]).is_none());
    }

    #[tokio::test]
    async fn suggest_missing_formula_matches_fetches_related_suggestions() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name":"python"},
                    {"name":"pytest"}
                ]"#,
            ))
            .expect(1)
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
            prefix,
            root.join("locks"),
        );

        let error = zb_core::Error::MissingFormula {
            name: "pythn".to_string(),
        };

        assert!(suggest_missing_formula_matches(&installer, &error).await);
    }

    #[tokio::test]
    async fn suggest_missing_formula_matches_returns_false_for_non_missing_errors() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::new();
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
            prefix,
            root.join("locks"),
        );

        let error = zb_core::Error::InvalidArgument {
            message: "bad formula".to_string(),
        };

        assert!(!suggest_missing_formula_matches(&installer, &error).await);
    }
}
