use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "zb")]
#[command(about = "Zerobrew - A fast Homebrew-compatible package installer")]
#[command(version)]
pub struct Cli {
    #[arg(long, env = "ZEROBREW_ROOT", help = "Path to zerobrew data directory")]
    pub root: Option<PathBuf>,

    #[arg(
        long,
        help = "Path to Homebrew-style prefix (overrides ZEROBREW_PREFIX)"
    )]
    pub prefix: Option<PathBuf>,

    #[arg(
        long,
        default_value = "20",
        value_parser = parse_concurrency,
        help = "Number of concurrent download threads"
    )]
    pub concurrency: usize,

    #[arg(
        long = "auto-init",
        global = true,
        env = "ZEROBREW_AUTO_INIT",
        help = "Automatically initialize data directories without prompting or modifying shell configuration"
    )]
    pub auto_init: bool,

    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count, help = "Increase output verbosity")]
    pub verbose: u8,

    #[arg(
        long,
        short = 'q',
        global = true,
        conflicts_with = "verbose",
        help = "Suppress output (only show errors)"
    )]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

impl Cli {
    /// Parses `args`, showing the help when no arguments are given.
    ///
    /// clap already does this for a required subcommand, but it counts options
    /// set from the environment as given. Once `zb init` has exported
    /// `ZEROBREW_ROOT`, a bare `zb` printed a missing-subcommand error instead
    /// of the help, so the environment is ignored in that case.
    pub fn try_parse_args<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let mut command = Self::command();
        if args.len() <= 1 {
            command = command.mut_args(|arg| arg.env(None));
        }
        let mut matches = command.try_get_matches_from(args)?;
        Self::from_arg_matches_mut(&mut matches).map_err(|e| e.format(&mut Self::command()))
    }
}

fn parse_concurrency(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid value '{}': expected a positive integer", value))?;
    if parsed == 0 {
        return Err("concurrency must be at least 1".to_string());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands};
    use clap::{Parser, error::ErrorKind};

    #[test]
    fn shows_help_without_arguments() {
        let err = Cli::try_parse_args(["zb"])
            .err()
            .expect("bare zb should not parse");
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn shows_help_without_arguments_when_env_options_are_set() {
        unsafe {
            std::env::set_var("ZEROBREW_AUTO_INIT", "true");
        }
        let result = Cli::try_parse_args(["zb"]);
        unsafe {
            std::env::remove_var("ZEROBREW_AUTO_INIT");
        }
        let err = result.err().expect("bare zb should not parse");
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn parses_subcommand_through_try_parse_args() {
        let cli = Cli::try_parse_args(["zb", "-v", "list"]).unwrap();
        assert_eq!(cli.verbose, 1);
        assert!(matches!(cli.command, super::Commands::List));
    }

    #[test]
    fn accepts_positive_concurrency() {
        let cli = Cli::try_parse_from(["zb", "--concurrency", "4", "list"]).unwrap();
        assert_eq!(cli.concurrency, 4);
    }

    #[test]
    fn rejects_zero_concurrency() {
        let result = Cli::try_parse_from(["zb", "--concurrency", "0", "list"]);
        assert!(result.is_err());
        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("at least 1"));
    }

    #[test]
    fn accepts_verbose_levels() {
        let cli = Cli::try_parse_from(["zb", "-vv", "list"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert!(!cli.quiet);
    }

    #[test]
    fn rejects_quiet_with_verbose() {
        let result = Cli::try_parse_from(["zb", "-v", "-q", "list"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_quiet_and_verbose_conflict() {
        let result = Cli::try_parse_from(["zb", "outdated", "--quiet", "--verbose"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_quiet_and_json_conflict() {
        let result = Cli::try_parse_from(["zb", "outdated", "--quiet", "--json"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_verbose_and_json_conflict() {
        let result = Cli::try_parse_from(["zb", "outdated", "--verbose", "--json"]);
        assert!(result.is_err());
    }

    #[test]
    fn install_parses_cask_flag_and_targets() {
        for (args, targets, expect_cask, expect_no_link) in [
            (&["--cask", "codex"][..], &["codex"][..], true, false),
            (
                &["codex", "claude-code", "--cask", "--no-link"],
                &["codex", "claude-code"],
                true,
                true,
            ),
            (
                &["jq", "homebrew/cask/codex", "cask:claude-code"],
                &["jq", "homebrew/cask/codex", "cask:claude-code"],
                false,
                false,
            ),
        ] {
            let cli =
                Cli::try_parse_args(["zb", "install"].into_iter().chain(args.iter().copied()))
                    .unwrap();
            let Commands::Install {
                formulas,
                cask,
                no_link,
                ..
            } = cli.command
            else {
                panic!("expected install command");
            };
            assert_eq!(formulas, targets);
            assert_eq!(cask, expect_cask);
            assert_eq!(no_link, expect_no_link);
        }
    }

    #[test]
    fn install_rejects_cask_with_source_build_or_without_targets() {
        for flag in ["-s", "--build-from-source"] {
            let err = Cli::try_parse_args(["zb", "install", "--cask", flag, "codex"])
                .err()
                .unwrap();
            assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
        }
        let err = Cli::try_parse_args(["zb", "install", "--cask"])
            .err()
            .unwrap();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }
}

#[derive(Subcommand)]
pub enum Commands {
    /// Enable or remove the opt-in brew compatibility entrypoint
    InstallAlias {
        #[arg(value_parser = ["brew"])]
        name: String,
        #[arg(long, help = "Remove only zerobrew's brew symlink")]
        remove: bool,
    },
    /// Install formulas and casks
    Install {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(
            long,
            conflicts_with = "build_from_source",
            help = "Treat all package arguments as casks"
        )]
        cask: bool,
        #[arg(long, help = "Do not create symlinks after installation")]
        no_link: bool,
        #[arg(long, short = 's', help = "Build from source instead of using bottles")]
        build_from_source: bool,
    },
    /// Install or dump from a Brewfile
    Bundle {
        #[command(subcommand)]
        command: Option<BundleCommands>,
    },
    /// Uninstall formulas and casks
    Uninstall {
        #[arg(required_unless_present = "all", num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long, help = "Uninstall all installed packages")]
        all: bool,
    },
    /// Migrate packages from Homebrew
    Migrate {
        #[arg(long, short = 'y', help = "Skip confirmation prompts")]
        yes: bool,
        #[arg(long, help = "Force uninstall from Homebrew even if errors occur")]
        force: bool,
    },
    /// List installed packages
    List,
    /// Show information about an installed package
    Info {
        #[arg(help = "Name of the installed package")]
        formula: String,
    },
    /// Run diagnostics and optionally repair issues
    Doctor {
        #[arg(long, help = "Automatically repair detected issues")]
        repair: bool,
    },
    /// Remove unreferenced store entries
    Gc,
    /// Reset zerobrew data directories
    Reset {
        #[arg(long, short = 'y', help = "Skip confirmation prompts")]
        yes: bool,
    },
    /// Initialize zerobrew directories
    Init {
        #[arg(long, help = "Do not modify shell configuration files")]
        no_modify_path: bool,
    },
    /// Generate shell completions
    Completion {
        #[arg(
            value_enum,
            help = "Target shell for completions (e.g., bash, zsh, fish)"
        )]
        shell: clap_complete::shells::Shell,
    },
    /// Write the zb man pages into directory
    Man {
        #[arg(
            long,
            value_name = "DIR",
            help = "Directory to write man pages into (zb.1, zb-<subcommand>.1)"
        )]
        out_dir: PathBuf,
    },
    /// Run an installed formula as a command
    Run {
        #[arg(help = "Name of the formula to run")]
        formula: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Refresh cached formula metadata
    Update,
    /// List installed packages with newer versions available
    Outdated {
        #[arg(long, conflicts_with_all = ["quiet", "verbose"], help = "Output as JSON")]
        json: bool,
    },
    /// Upgrade installed packages to the latest versions
    Upgrade {
        #[arg(required = false, num_args = 0..)]
        formulas: Vec<String>,
        #[arg(long, short = 's', help = "Build from source instead of using bottles")]
        build_from_source: bool,
        #[arg(long, help = "Do not create symlinks after installation")]
        no_link: bool,
    },
    /// Run the post-install steps of installed packages again
    Postinstall {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum BundleCommands {
    /// Install packages from a Brewfile
    Install {
        #[arg(
            long,
            short = 'f',
            value_name = "FILE",
            default_value = "Brewfile",
            help = "Path to the Brewfile"
        )]
        file: PathBuf,
        #[arg(long, help = "Do not create symlinks after installation")]
        no_link: bool,
    },
    /// Dump installed packages to a Brewfile
    Dump {
        #[arg(
            long,
            short = 'f',
            value_name = "FILE",
            default_value = "Brewfile",
            help = "Output file path"
        )]
        file: PathBuf,
        #[arg(long, help = "Overwrite existing file")]
        force: bool,
    },
}
