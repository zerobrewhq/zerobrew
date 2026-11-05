use console::style;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::install;
use crate::cli::BundleCommands;
use crate::ui::StdUi;

pub async fn execute(
    installer: &mut zb_io::Installer,
    command: Option<BundleCommands>,
    ui: &mut StdUi,
) -> Result<(), zb_core::Error> {
    match command.unwrap_or(BundleCommands::Install {
        file: PathBuf::from("Brewfile"),
        no_link: false,
    }) {
        BundleCommands::Install { file, no_link } => {
            install_from_file(installer, &file, no_link, ui).await
        }
        BundleCommands::Dump { file, force } => dump_to_file(installer, &file, force),
    }
}

async fn install_from_file(
    installer: &mut zb_io::Installer,
    manifest_path: &Path,
    no_link: bool,
    ui: &mut StdUi,
) -> Result<(), zb_core::Error> {
    let formulas = load_manifest(manifest_path)?;
    println!(
        "{} Installing {} formulas from {}...",
        style("==>").cyan().bold(),
        style(formulas.len()).green().bold(),
        manifest_path.display()
    );

    let start = Instant::now();
    let mut post_install_failed = Vec::new();
    for formula in formulas {
        let result = install::execute(installer, vec![formula.clone()], no_link, false, ui).await;
        if let Some(err) =
            keep_going_after_post_install(result, &formula, &mut post_install_failed)?
        {
            ui.error(err).map_err(ui_error)?;
        }
    }

    println!(
        "{} Finished installing manifest in {:.2}s",
        style("==>").cyan().bold(),
        start.elapsed().as_secs_f64()
    );

    if post_install_failed.is_empty() {
        return Ok(());
    }
    Err(zb_core::Error::ExecutionError {
        message: format!(
            "post-install did not finish for: {}",
            post_install_failed.join(", ")
        ),
    })
}

/// A failed post-install leaves the package installed, so the rest of the
/// manifest should still be installed. Records the entry and hands the error
/// back for printing; any other error stops the run.
fn keep_going_after_post_install(
    result: Result<(), zb_core::Error>,
    entry: &str,
    post_install_failed: &mut Vec<String>,
) -> Result<Option<zb_core::Error>, zb_core::Error> {
    match result {
        Ok(()) => Ok(None),
        Err(err @ zb_core::Error::PostInstallFailed { .. }) => {
            post_install_failed.push(entry.to_string());
            Ok(Some(err))
        }
        Err(err) => Err(err),
    }
}

fn ui_error(err: std::io::Error) -> zb_core::Error {
    zb_core::Error::FileError {
        message: format!("failed to write CLI output: {err}"),
    }
}

fn dump_to_file(
    installer: &mut zb_io::Installer,
    file_path: &Path,
    force: bool,
) -> Result<(), zb_core::Error> {
    if file_path.exists() && !force {
        return Err(zb_core::Error::FileError {
            message: format!(
                "file {} already exists (use --force to overwrite)",
                file_path.display()
            ),
        });
    }

    let installed = installer.list_installed()?;
    let mut content = String::new();
    for keg in &installed {
        content.push_str(&format!("brew \"{}\"\n", keg.name));
    }

    std::fs::write(file_path, content).map_err(|e| zb_core::Error::FileError {
        message: format!("failed to write {}: {}", file_path.display(), e),
    })?;

    println!(
        "{} Dumped {} packages to {}",
        style("==>").cyan().bold(),
        style(installed.len()).green().bold(),
        file_path.display()
    );

    Ok(())
}

fn load_manifest(path: &Path) -> Result<Vec<String>, zb_core::Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| zb_core::Error::FileError {
        message: format!("failed to read manifest {}: {}", path.display(), e),
    })?;

    let mut formulas = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for line in contents.lines() {
        // Handle inline comments by splitting on '#' and taking the first part
        let entry = line.split('#').next().unwrap_or("").trim();
        if entry.is_empty() {
            continue;
        }

        if let Some(parsed) = parse_brewfile_entry(entry)
            && seen.insert(parsed.clone())
        {
            formulas.push(parsed);
        }
    }

    if formulas.is_empty() {
        return Err(zb_core::Error::FileError {
            message: format!("manifest {} did not contain any formulas", path.display()),
        });
    }

    Ok(formulas)
}

fn parse_brewfile_entry(line: &str) -> Option<String> {
    if line.starts_with("tap ") {
        return None;
    }

    if let Some(token) = parse_quoted_directive(line, "cask") {
        return Some(format!("cask:{token}"));
    }

    if let Some(formula) = parse_quoted_directive(line, "brew") {
        return Some(formula.to_string());
    }

    Some(line.to_string())
}

fn parse_quoted_directive<'a>(line: &'a str, directive: &str) -> Option<&'a str> {
    if !line.starts_with(directive) {
        return None;
    }

    let rest = line[directive.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let tail = &rest[1..];
    let end = tail.find(quote)?;
    Some(&tail[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_manifest_parses_entries_ignoring_whitespace_and_comments() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "# comment\n\njq\nwget\njq\n   git  \n# another comment"
        )
        .unwrap();

        let entries = load_manifest(file.path()).unwrap();
        assert_eq!(entries, vec!["jq", "wget", "git"]);
    }

    #[test]
    fn load_manifest_handles_inline_comments() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "jq # inline comment\nwget# no space\n  git  # with spaces  "
        )
        .unwrap();

        let entries = load_manifest(file.path()).unwrap();
        assert_eq!(entries, vec!["jq", "wget", "git"]);
    }

    #[test]
    fn load_manifest_errors_when_only_comments() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "# nothing here\n   # still nothing").unwrap();

        let err = load_manifest(file.path()).unwrap_err();
        match err {
            zb_core::Error::FileError { message } => {
                assert!(message.contains("did not contain any formulas"))
            }
            other => panic!("expected file error, got {other:?}"),
        }
    }

    #[test]
    fn load_manifest_errors_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        let err = load_manifest(&missing).unwrap_err();
        match err {
            zb_core::Error::FileError { message } => {
                assert!(message.contains("failed to read manifest"))
            }
            other => panic!("expected file error, got {other:?}"),
        }
    }

    #[test]
    fn load_manifest_parses_brewfile_cask_and_brew_entries() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "tap \"homebrew/cask\"\nbrew \"wget\"\ncask \"docker-desktop\"\n"
        )
        .unwrap();

        let entries = load_manifest(file.path()).unwrap();
        assert_eq!(entries, vec!["wget", "cask:docker-desktop"]);
    }

    #[test]
    fn parse_brewfile_entry_handles_brew_directive() {
        assert_eq!(parse_brewfile_entry("brew \"jq\""), Some("jq".to_string()));
        assert_eq!(
            parse_brewfile_entry("brew 'wget'"),
            Some("wget".to_string())
        );
    }

    #[test]
    fn parse_brewfile_entry_handles_cask_directive() {
        assert_eq!(
            parse_brewfile_entry("cask \"docker\""),
            Some("cask:docker".to_string())
        );
    }

    #[test]
    fn post_install_failure_does_not_stop_the_manifest() {
        let mut failed = Vec::new();
        let err = keep_going_after_post_install(
            Err(zb_core::Error::PostInstallFailed {
                name: "php".to_string(),
                step: "configure_php".to_string(),
                message: "not supported".to_string(),
            }),
            "composer",
            &mut failed,
        )
        .unwrap();
        assert!(matches!(
            err,
            Some(zb_core::Error::PostInstallFailed { .. })
        ));
        assert_eq!(failed, vec!["composer"]);

        assert_eq!(
            keep_going_after_post_install(Ok(()), "jq", &mut failed).unwrap(),
            None
        );
        assert_eq!(failed, vec!["composer"]);
    }

    #[test]
    fn other_install_errors_still_stop_the_manifest() {
        let mut failed = Vec::new();
        let err = keep_going_after_post_install(
            Err(zb_core::Error::MissingFormula {
                name: "nope".to_string(),
            }),
            "nope",
            &mut failed,
        )
        .unwrap_err();
        assert!(matches!(err, zb_core::Error::MissingFormula { .. }));
        assert!(failed.is_empty());
    }

    #[test]
    fn parse_brewfile_entry_skips_tap_directive() {
        assert_eq!(parse_brewfile_entry("tap \"homebrew/core\""), None);
    }
}
