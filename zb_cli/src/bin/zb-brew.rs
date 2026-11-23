use clap::{Parser, error::ErrorKind};
use std::{env, ffi::OsString, process::Command};
use zb_cli::cli::Cli;

const HELP: &str = "brew - zerobrew's opt-in Homebrew compatibility entrypoint

Usage: brew <command> [args...]

Supported commands: install, uninstall, list, info, outdated, upgrade, doctor,
bundle, update, cleanup (zerobrew gc only).
Only arguments supported by zb are accepted, including `install --cask`.
`list --cask`, `list --formula`, and search are not implemented.
Package information and output follow zb's semantics.
Use `brew <command> --help` for supported arguments.
Remove this entrypoint with `zb install-alias brew --remove`.";

fn translate(args: &mut [OsString]) -> Result<(), String> {
    let command = args[0].to_str().unwrap_or_default();
    match command {
        "install" | "uninstall" | "list" | "info" | "outdated" | "upgrade" | "doctor"
        | "bundle" | "update" => Ok(()),
        "cleanup" => {
            args[0] = "gc".into();
            Ok(())
        }
        _ => Err(format!(
            "`brew {}` is not implemented by zerobrew; use `brew --help` for supported commands",
            args[0].to_string_lossy()
        )),
    }
}

fn main() {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    if args.is_empty() || matches!(args[0].to_str(), Some("--help" | "-h" | "help")) {
        println!("{HELP}");
        return;
    }
    if matches!(args[0].to_str(), Some("--version" | "-v")) && args.len() == 1 {
        println!("zerobrew brew compatibility {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cleanup = args[0] == "cleanup";
    if let Err(e) = translate(&mut args) {
        eprintln!("error: {e}");
        std::process::exit(2);
    }
    // Validate before exec so unsupported Homebrew flags get a compatibility error.
    if let Err(e) = Cli::try_parse_from(std::iter::once(OsString::from("zb")).chain(args.clone())) {
        if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
            println!("zerobrew brew compatibility\n{e}");
            return;
        }
        eprintln!(
            "error: arguments are not supported by zerobrew's brew compatibility entrypoint\n{e}"
        );
        std::process::exit(2);
    }
    if cleanup {
        eprintln!(
            "zerobrew: cleanup runs `zb gc`; only unreferenced store entries are removed, not Homebrew's cache cleanup."
        );
    }
    let result = run_zb(&args);
    eprintln!("error: cannot execute adjacent zb: {result}; reinstall zerobrew");
    std::process::exit(1);
}

fn run_zb(args: &[OsString]) -> std::io::Error {
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return e,
    };
    let Some(dir) = exe.parent() else {
        return std::io::Error::other("cannot locate zb-brew's directory");
    };
    let mut cmd = Command::new(dir.join("zb"));
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_cleanup_and_keeps_arguments_exactly() {
        let mut args = vec![OsString::from("cleanup"), OsString::from("--quiet")];
        translate(&mut args).unwrap();
        assert_eq!(args, ["gc", "--quiet"]);
        let mut args = vec![
            OsString::from("install"),
            OsString::from("a b"),
            OsString::from("--"),
            OsString::from("-x"),
        ];
        let original = args.clone();
        translate(&mut args).unwrap();
        assert_eq!(args, original);
    }

    #[test]
    fn rejects_unsupported_commands_before_running_zb() {
        for command in [
            "search", "services", "tap", "deps", "migrate", "reset", "leaves",
        ] {
            assert!(
                translate(&mut [command.into()])
                    .unwrap_err()
                    .contains("not implemented by zerobrew")
            );
        }
    }
}
