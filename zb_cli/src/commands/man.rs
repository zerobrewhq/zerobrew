use clap::CommandFactory;
use std::io;

pub fn execute() -> Result<(), zb_core::Error> {
    let cmd = crate::cli::Cli::command();
    let man = clap_mangen::Man::new(cmd);
    man.render(&mut io::stdout())
        .map_err(|e| zb_core::Error::FileError {
            message: e.to_string(),
        })?;
    Ok(())
}
