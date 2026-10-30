use console::style;

use crate::ui::StdUi;

pub fn execute(
    installer: &mut zb_io::Installer,
    repair: bool,
    ui: &mut StdUi,
) -> Result<(), zb_core::Error> {
    ui.heading("Running diagnostics...").map_err(ui_error)?;

    let report = installer.doctor()?;

    #[cfg(target_os = "android")]
    for msg in zb_io::android_doctor_messages() {
        ui.warn(msg).map_err(ui_error)?;
    }

    if report.is_healthy() {
        ui.println(format!("    {} No issues found", style("✓").green()))
            .map_err(ui_error)?;
        return Ok(());
    }

    for orphan in &report.orphaned_cellar_kegs {
        ui.warn(format!(
            "Orphaned cellar keg: {}/{} (no DB record)",
            orphan.name, orphan.version
        ))
        .map_err(ui_error)?;
    }

    for missing in &report.missing_cellar_kegs {
        ui.warn(format!(
            "Missing cellar keg: {}/{} (DB record exists but {} is gone)",
            missing.name,
            missing.version,
            missing.expected_path.display()
        ))
        .map_err(ui_error)?;
    }

    for misnamed in &report.misnamed_records {
        ui.warn(format!(
            "Misnamed record: {} {} (installed as {})",
            misnamed.recorded_name, misnamed.version, misnamed.actual_name
        ))
        .map_err(ui_error)?;
    }

    for key in &report.orphaned_store_entries {
        ui.warn(format!(
            "Orphaned store entry: {} (no DB reference)",
            &key[..key.len().min(12)]
        ))
        .map_err(ui_error)?;
    }

    for stale in &report.stale_store_refs {
        let status = if stale.missing_store_entry() {
            "not on disk"
        } else if !stale.referenced_by_any_keg {
            "unreferenced"
        } else {
            "refcount mismatch"
        };
        ui.warn(format!(
            "Stale store ref: {} (refcount={}, {})",
            &stale.store_key[..stale.store_key.len().min(12)],
            stale.refcount,
            status
        ))
        .map_err(ui_error)?;
    }

    for link in &report.broken_symlinks {
        ui.warn(format!("Broken symlink: {}", link.display()))
            .map_err(ui_error)?;
    }

    if report.stale_keg_file_records > 0 {
        ui.warn(format!(
            "{} stale keg_files records (referencing uninstalled kegs)",
            report.stale_keg_file_records
        ))
        .map_err(ui_error)?;
    }

    for broken in &report.broken_linkage {
        let owner = broken
            .dependency
            .as_ref()
            .map(|dependency| format!(" from {dependency}"))
            .unwrap_or_default();
        ui.warn(format!(
            "Broken linkage: {} {} loads {}{owner}, which is missing",
            broken.name,
            broken.version,
            broken.library.display()
        ))
        .map_err(ui_error)?;
    }

    let issue_count = report.orphaned_cellar_kegs.len()
        + report.missing_cellar_kegs.len()
        + report.misnamed_records.len()
        + report.orphaned_store_entries.len()
        + report.stale_store_refs.len()
        + report.broken_symlinks.len()
        + report.broken_linkage.len()
        + usize::from(report.stale_keg_file_records > 0);

    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Found {} {}",
        style(issue_count).yellow().bold(),
        if issue_count == 1 { "issue" } else { "issues" }
    ))
    .map_err(ui_error)?;

    if !report.broken_linkage.is_empty() {
        ui.println(format!(
            "    Broken linkage isn't repaired automatically. Run {} on the package or on the dependency it loads from",
            style("zb upgrade <package>").bold()
        ))
        .map_err(ui_error)?;
    }

    if !report.has_repairable_issues() {
        return Ok(());
    }

    if !repair {
        ui.println(format!(
            "    Run {} to fix{}",
            style("zb doctor --repair").bold(),
            if report.broken_linkage.is_empty() {
                ""
            } else {
                " the rest"
            }
        ))
        .map_err(ui_error)?;
        return Ok(());
    }

    ui.blank_line().map_err(ui_error)?;
    ui.heading("Repairing...").map_err(ui_error)?;

    let (summary, remaining) = installer.repair_until_healthy(report)?;

    if summary.renamed_records > 0 {
        ui.bullet(format!(
            "Renamed {} alias {}",
            summary.renamed_records,
            pluralize("record", summary.renamed_records)
        ))
        .map_err(ui_error)?;
    }
    if summary.removed_orphaned_kegs > 0 {
        ui.bullet(format!(
            "Removed {} orphaned cellar {}",
            summary.removed_orphaned_kegs,
            pluralize("keg", summary.removed_orphaned_kegs)
        ))
        .map_err(ui_error)?;
    }
    if summary.removed_missing_records > 0 {
        ui.bullet(format!(
            "Removed {} stale DB {}",
            summary.removed_missing_records,
            pluralize("record", summary.removed_missing_records)
        ))
        .map_err(ui_error)?;
    }
    if summary.fixed_store_refs > 0 {
        ui.bullet(format!(
            "Fixed {} store {}",
            summary.fixed_store_refs,
            pluralize("ref", summary.fixed_store_refs)
        ))
        .map_err(ui_error)?;
    }
    if summary.removed_orphaned_store_entries > 0 {
        ui.bullet(format!(
            "Removed {} orphaned store {}",
            summary.removed_orphaned_store_entries,
            pluralize("entry", summary.removed_orphaned_store_entries)
        ))
        .map_err(ui_error)?;
    }
    if summary.removed_broken_symlinks > 0 {
        ui.bullet(format!(
            "Removed {} broken {}",
            summary.removed_broken_symlinks,
            pluralize("symlink", summary.removed_broken_symlinks)
        ))
        .map_err(ui_error)?;
    }
    if summary.pruned_keg_file_records > 0 {
        ui.bullet(format!(
            "Pruned {} stale keg_files {}",
            summary.pruned_keg_file_records,
            pluralize("record", summary.pruned_keg_file_records)
        ))
        .map_err(ui_error)?;
    }

    ui.blank_line().map_err(ui_error)?;
    ui.println(format!(
        "    {} Applied {} {}",
        style("✓").green(),
        summary.total_fixes(),
        pluralize("fix", summary.total_fixes())
    ))
    .map_err(ui_error)?;

    if !remaining.is_healthy() {
        ui.warn("Some issues couldn't be repaired. Run zb doctor for details.")
            .map_err(ui_error)?;
    }

    Ok(())
}

fn pluralize(word: &str, count: usize) -> &str {
    if count == 1 {
        word
    } else {
        match word {
            "keg" => "kegs",
            "record" => "records",
            "ref" => "refs",
            "entry" => "entries",
            "symlink" => "symlinks",
            "fix" => "fixes",
            "issue" => "issues",
            _ => word,
        }
    }
}

fn ui_error(err: std::io::Error) -> zb_core::Error {
    zb_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
