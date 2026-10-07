use console::style;

pub async fn execute(installer: &mut zb_io::Installer) -> Result<(), zb_core::Error> {
    let formulas = installer.update_api_cache().await?;
    println!(
        "{} Refreshed the formula index: {} formulas.",
        style("==>").cyan().bold(),
        style(formulas).green().bold()
    );
    println!(
        "{}",
        style("Run `zb outdated` to check package updates.").dim()
    );
    println!(
        "{}",
        style("This does not update the zb binary; use the installer or Homebrew for that.").dim()
    );
    Ok(())
}
