use crate::ui::{PromptDefault, StdUi};
use console::style;
use std::process::Command;

pub async fn execute(
    installer: &mut zb_io::Installer,
    yes: bool,
    force: bool,
    ui: &mut StdUi,
) -> Result<(), zb_core::Error> {
    ui.heading("Fetching installed Homebrew packages...")
        .map_err(ui_error)?;

    let brew = zb_io::installer::homebrew::find_homebrew()?;
    let packages = zb_io::get_homebrew_packages(&brew)?;

    if packages.formulas.is_empty()
        && packages.non_core_formulas.is_empty()
        && packages.casks.is_empty()
    {
        ui.println("No Homebrew packages installed.")
            .map_err(ui_error)?;
        return Ok(());
    }

    ui.println(format!(
        "{} core formulas, {} non-core formulas, {} casks found",
        style(packages.formulas.len()).green(),
        style(packages.non_core_formulas.len()).yellow(),
        style(packages.casks.len()).green()
    ))
    .map_err(ui_error)?;
    ui.blank_line().map_err(ui_error)?;

    if !packages.non_core_formulas.is_empty() {
        ui.note("Formulas from non-core taps cannot be migrated to zerobrew:")
            .map_err(ui_error)?;
        for pkg in &packages.non_core_formulas {
            ui.bullet(format!("{} ({})", pkg.name, pkg.tap))
                .map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    if !packages.casks.is_empty() {
        ui.note("Casks cannot be migrated to zerobrew (only CLI formulas are supported):")
            .map_err(ui_error)?;
        for cask in &packages.casks {
            ui.bullet(&cask.name).map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    if packages.formulas.is_empty() {
        ui.println("No core formulas to migrate.")
            .map_err(ui_error)?;
        return Ok(());
    }

    ui.println(format!(
        "The following {} formulas will be migrated:",
        packages.formulas.len()
    ))
    .map_err(ui_error)?;
    for pkg in &packages.formulas {
        ui.bullet(&pkg.name).map_err(ui_error)?;
    }
    ui.blank_line().map_err(ui_error)?;

    if !yes
        && !ui
            .prompt_yes_no("Continue with migration? [y/N]", PromptDefault::No)
            .map_err(ui_error)?
    {
        ui.println("Aborted.").map_err(ui_error)?;
        return Ok(());
    }

    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Migrating {} formulas to zerobrew...",
        style(packages.formulas.len()).green().bold()
    ))
    .map_err(ui_error)?;

    let formula_names: Vec<String> = packages.formulas.iter().map(|f| f.name.clone()).collect();

    let (plan, planning_failures) = installer.plan_best_effort(&formula_names, false).await;
    if !planning_failures.is_empty() {
        ui.note(format!(
            "Skipped {} formula(s) that could not be planned:",
            planning_failures.len()
        ))
        .map_err(ui_error)?;
        for failure in &planning_failures {
            ui.bullet(format!("{} ({})", failure.name, failure.error))
                .map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    let post_install_failed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    if !plan.items.is_empty() {
        crate::commands::install::execute_formula_plan_tracking(
            installer,
            &formula_names,
            plan,
            false, // no_link
            ui,
            post_install_failed.clone(),
        )
        .await
        .ok();
    }
    let post_install_failed = post_install_failed.lock().unwrap().clone();

    let InstallStatus {
        migrated: successfully_installed,
        not_installed: failed_installed,
        post_install_incomplete,
    } = check_install_status(installer, &formula_names, &post_install_failed)?;
    let success_count = successfully_installed.len();

    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Migrated {} of {} formulas to zerobrew",
        style(success_count).green().bold(),
        packages.formulas.len()
    ))
    .map_err(ui_error)?;

    if !failed_installed.is_empty() {
        ui.note(format!(
            "Failed to migrate {} formula(s):",
            failed_installed.len()
        ))
        .map_err(ui_error)?;
        for name in &failed_installed {
            ui.bullet(name).map_err(ui_error)?;
        }
        ui.blank_line().map_err(ui_error)?;
    }

    if !post_install_incomplete.is_empty() {
        ui.note(format!(
            "{} formula(s) installed but their post-install steps did not finish, so they were not counted as migrated and will stay in Homebrew:",
            post_install_incomplete.len()
        ))
        .map_err(ui_error)?;
        for name in &post_install_incomplete {
            ui.bullet(name).map_err(ui_error)?;
        }
        ui.println("Retry with `zb postinstall <formula>`.")
            .map_err(ui_error)?;
        ui.blank_line().map_err(ui_error)?;
    }

    if success_count == 0 {
        ui.println("No formulas were successfully migrated. Skipping uninstall from Homebrew.")
            .map_err(ui_error)?;
        return Ok(());
    }

    ui.blank_line().map_err(ui_error)?;
    if !yes
        && !ui
            .prompt_yes_no(
                &format!(
                    "Uninstall {} formula(s) from Homebrew? [y/N]",
                    style(success_count).green()
                ),
                PromptDefault::No,
            )
            .map_err(ui_error)?
    {
        ui.println("Skipped uninstall from Homebrew.")
            .map_err(ui_error)?;
        return Ok(());
    }

    ui.blank_line().map_err(ui_error)?;
    ui.heading("Uninstalling from Homebrew...")
        .map_err(ui_error)?;

    if successfully_installed.is_empty() {
        return Ok(());
    }

    ui.step_start(format!(
        "uninstalling {} formulas combined",
        successfully_installed.len()
    ))
    .map_err(ui_error)?;

    let mut args = vec!["uninstall"];
    if force {
        args.push("--force");
    }
    for target in &successfully_installed {
        args.push(target);
    }

    let status = Command::new(&brew)
        .args(&args)
        .status()
        .map_err(|e| format!("Failed to run brew uninstall: {}", e));

    let uninstall_failed = match status {
        Ok(s) if s.success() => {
            ui.step_ok().map_err(ui_error)?;
            Vec::new()
        }
        res => {
            ui.step_fail().map_err(ui_error)?;
            if let Err(e) = res {
                ui.error(e).map_err(ui_error)?;
            }
            let mut actually_failed = successfully_installed.clone();
            if let Ok(output) = Command::new(&brew).args(["list", "--formula"]).output()
                && output.status.success()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let still_installed: std::collections::HashSet<&str> = stdout.lines().collect();
                actually_failed.retain(|target| still_installed.contains(target.as_str()));
            }
            actually_failed
        }
    };

    let uninstalled = successfully_installed.len() - uninstall_failed.len();
    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Uninstalled {} of {} formula(s) from Homebrew",
        style(uninstalled).green().bold(),
        success_count
    ))
    .map_err(ui_error)?;

    if !uninstall_failed.is_empty() {
        ui.note(format!(
            "Failed to uninstall {} formula(s) from Homebrew:",
            uninstall_failed.len()
        ))
        .map_err(ui_error)?;
        for name in &uninstall_failed {
            ui.bullet(name).map_err(ui_error)?;
        }
        ui.println("You may need to uninstall these manually with:")
            .map_err(ui_error)?;
        ui.println("    brew uninstall --force <formula>")
            .map_err(ui_error)?;
    }

    Ok(())
}

/// Where each requested formula ended up. Only `migrated` is safe to
/// uninstall from Homebrew.
#[derive(Debug, Default, PartialEq, Eq)]
struct InstallStatus {
    migrated: Vec<String>,
    not_installed: Vec<String>,
    /// Installed, but post-install steps did not finish.
    post_install_incomplete: Vec<String>,
}

fn check_install_status(
    installer: &zb_io::Installer,
    formula_names: &[String],
    post_install_failed: &[String],
) -> Result<InstallStatus, zb_core::Error> {
    let installed_kegs =
        installer
            .list_installed()
            .map_err(|e| zb_core::Error::StoreCorruption {
                message: format!("Failed to verify installation status: {}", e),
            })?;

    let installed_names: std::collections::HashSet<String> =
        installed_kegs.into_iter().map(|k| k.name).collect();
    Ok(partition_install_status(
        formula_names,
        &installed_names,
        post_install_failed,
    ))
}

fn partition_install_status(
    formula_names: &[String],
    installed_names: &std::collections::HashSet<String>,
    post_install_failed: &[String],
) -> InstallStatus {
    let mut status = InstallStatus::default();
    for name in formula_names {
        if !installed_names.contains(name) {
            status.not_installed.push(name.clone());
        } else if post_install_failed.contains(name) {
            status.post_install_incomplete.push(name.clone());
        } else {
            status.migrated.push(name.clone());
        }
    }
    status
}

fn ui_error(err: std::io::Error) -> zb_core::Error {
    zb_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn post_install_failure_is_not_counted_as_migrated() {
        let formulas = names(&["wget", "php", "jq"]);
        let installed: std::collections::HashSet<String> =
            ["wget", "php"].iter().map(|s| s.to_string()).collect();
        let status = partition_install_status(&formulas, &installed, &names(&["php"]));
        assert_eq!(status.migrated, names(&["wget"]));
        assert_eq!(status.not_installed, names(&["jq"]));
        assert_eq!(status.post_install_incomplete, names(&["php"]));
    }
}
