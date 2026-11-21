use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use zb_io::{InstallProgress, ProgressCallback};

use crate::ui::StdUi;
use crate::utils::normalize_formula_name;

pub async fn execute(
    installer: &mut zb_io::Installer,
    formulas: Vec<String>,
    build_from_source: bool,
    no_link: bool,
    ui: &mut StdUi,
) -> Result<(), zb_core::Error> {
    let start = Instant::now();

    // Reported at the end so explicit-arg upgrades still run; we exit
    // non-zero afterwards rather than discard partial progress.
    let mut missing: Vec<String> = Vec::new();

    let outdated = if formulas.is_empty() {
        ui.heading("Checking for outdated packages...".to_string())
            .map_err(ui_error)?;
        let (outdated, warnings) = installer.check_outdated().await?;
        for warning in &warnings {
            eprintln!("{} {}", style("Warning:").yellow().bold(), warning);
        }
        if outdated.is_empty() {
            ui.info("All packages are up to date.".to_string())
                .map_err(ui_error)?;
            return Ok(());
        }
        outdated
    } else {
        let mut normalized = Vec::with_capacity(formulas.len());
        for formula in &formulas {
            normalized.push(normalize_formula_name(formula)?);
        }
        let mut outdated = Vec::new();
        for name in &normalized {
            match installer.is_outdated(name).await {
                Ok(Some(pkg)) => outdated.push(pkg),
                Ok(None) => {
                    ui.info(format!("{} is already up to date", name))
                        .map_err(ui_error)?;
                }
                Err(zb_core::Error::NotInstalled { .. }) => {
                    ui.error(format!("{} is not installed", name))
                        .map_err(ui_error)?;
                    missing.push(name.clone());
                }
                Err(e) => return Err(e),
            }
        }
        if outdated.is_empty() {
            if let Some(name) = missing.into_iter().next() {
                return Err(zb_core::Error::NotInstalled { name });
            }
            ui.info("All specified packages are up to date.".to_string())
                .map_err(ui_error)?;
            return Ok(());
        }
        outdated
    };

    ui.heading(format!("Upgrading {}...", style(outdated.len()).bold()))
        .map_err(ui_error)?;

    let multi = MultiProgress::new();
    let bars: Arc<Mutex<HashMap<String, ProgressBar>>> = Arc::new(Mutex::new(HashMap::new()));

    let spinner_style = ProgressStyle::default_spinner()
        .template("    {prefix:<16} {spinner:.cyan} {msg}")
        .unwrap()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

    let done_style = ProgressStyle::default_spinner()
        .template("    {prefix:<16} {msg}")
        .unwrap();

    let bars_clone = bars.clone();
    let multi_clone = multi.clone();
    let spinner_style_clone = spinner_style.clone();
    let done_style_clone = done_style.clone();

    let progress_callback: Arc<ProgressCallback> = Arc::new(Box::new(move |event| {
        let mut bars = bars_clone.lock().unwrap();
        match event {
            InstallProgress::DownloadStarted { name, total_bytes } => {
                let pb = if let Some(total) = total_bytes {
                    let pb = multi_clone.add(ProgressBar::new(total));
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template("    {prefix:<16} {bar:25.cyan/dim} {bytes:>10}/{total_bytes:<10} {eta:>6}")
                            .unwrap()
                            .progress_chars("━━╸"),
                    );
                    pb
                } else {
                    let pb = multi_clone.add(ProgressBar::new_spinner());
                    pb.set_style(spinner_style_clone.clone());
                    pb.set_message("downloading...");
                    pb.enable_steady_tick(std::time::Duration::from_millis(80));
                    pb
                };
                pb.set_prefix(name.clone());
                bars.insert(name, pb);
            }
            InstallProgress::DownloadProgress {
                name,
                downloaded,
                total_bytes,
            } => {
                if let Some(pb) = bars.get(&name)
                    && total_bytes.is_some()
                {
                    pb.set_position(downloaded);
                }
            }
            InstallProgress::DownloadCompleted { name, total_bytes } => {
                if let Some(pb) = bars.get(&name) {
                    if total_bytes > 0 {
                        pb.set_position(total_bytes);
                    }
                    pb.set_style(spinner_style_clone.clone());
                    pb.set_message("unpacking...");
                    pb.enable_steady_tick(std::time::Duration::from_millis(80));
                }
            }
            InstallProgress::UnpackStarted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("unpacking...");
                }
            }
            InstallProgress::UnpackCompleted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("unpacked");
                }
            }
            InstallProgress::LinkStarted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("linking...");
                }
            }
            InstallProgress::LinkCompleted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message("linked");
                }
            }
            InstallProgress::LinkSkipped { name, reason } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_message(format!("keg-only ({})", reason));
                }
            }
            InstallProgress::InstallCompleted { name } => {
                if let Some(pb) = bars.get(&name) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} upgraded", style("✓").green()));
                    pb.finish();
                }
            }
            InstallProgress::PostInstallStarted { name } => {
                let pb = multi_clone.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style_clone.clone());
                pb.set_prefix(name.clone());
                pb.set_message("running post-install...");
                pb.enable_steady_tick(std::time::Duration::from_millis(80));
                bars.insert(format!("{name} post-install"), pb);
            }
            InstallProgress::PostInstallCompleted { name } => {
                if let Some(pb) = bars.get(&format!("{name} post-install")) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} post-install done", style("✓").green()));
                    pb.finish();
                }
            }
            InstallProgress::PostInstallFailed { name, .. } => {
                if let Some(pb) = bars.get(&format!("{name} post-install")) {
                    pb.set_style(done_style_clone.clone());
                    pb.set_message(format!("{} post-install failed", style("✗").red()));
                    pb.finish();
                }
            }
        }
    }));

    let mut upgraded = 0usize;
    let mut errors: Vec<(String, zb_core::Error)> = Vec::new();
    let mut notices = Vec::new();
    let mut post_install_failures = Vec::new();
    let mut caveats = Vec::new();

    for pkg in &outdated {
        let name = &pkg.name;
        ui.step_start(name).map_err(ui_error)?;

        match installer
            .upgrade(
                name,
                build_from_source,
                !no_link,
                Some(progress_callback.clone()),
            )
            .await
        {
            Ok(result) => {
                ui.step_ok().map_err(ui_error)?;
                upgraded += 1;
                notices.extend(result.notices);
                post_install_failures.extend(result.post_install_failures);
                caveats.extend(result.caveats);
            }
            Err(e) => {
                ui.step_fail().map_err(ui_error)?;
                errors.push((name.clone(), e));
            }
        }
    }

    {
        let bars = bars.lock().unwrap();
        for (_, pb) in bars.iter() {
            if !pb.is_finished() {
                pb.finish();
            }
        }
    }

    let elapsed = start.elapsed();
    ui.blank_line().map_err(ui_error)?;

    for (name, err) in &errors {
        ui.error(format!("Failed to upgrade {}: {}", style(name).bold(), err))
            .map_err(ui_error)?;
    }

    if errors.is_empty() && missing.is_empty() {
        ui.heading(format!(
            "Upgraded {} packages in {:.2}s",
            style(upgraded).green().bold(),
            elapsed.as_secs_f64()
        ))
        .map_err(ui_error)?;
        super::install::report_caveats(ui, &caveats).map_err(ui_error)?;
        super::install::report_post_install(ui, &notices, post_install_failures)
    } else if !errors.is_empty() {
        Err(errors.remove(0).1)
    } else {
        Err(zb_core::Error::NotInstalled {
            name: missing.remove(0),
        })
    }
}

fn ui_error(err: std::io::Error) -> zb_core::Error {
    zb_core::Error::FileError {
        message: format!("failed to write CLI output: {err}"),
    }
}
