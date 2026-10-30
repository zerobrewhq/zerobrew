//! Derived from Homebrew's `Library/Homebrew/install_steps.rb` and
//! `install_steps/formula_actions.rb` (https://github.com/Homebrew/brew).
//! Copyright (c) 2009-present, Homebrew contributors. BSD 2-Clause License,
//! see LICENSE-HOMEBREW.
//!
//! Run a formula's post-install steps.
//!
//! Homebrew runs these once a keg is poured and linked: building the CA
//! bundle for `ca-certificates`, copying `npm` out of node's keg, making
//! python's venv scripts writable. The steps come from the formula API as
//! data (see `zb_core::PostInstallStep`); this module gives them meaning,
//! following `Library/Homebrew/install_steps.rb` so a prefix set up by
//! zerobrew ends up the same as one set up by Homebrew.
//!
//! A failed step does not undo the install. The keg stays in place and
//! linked, the failure is reported, and `zb postinstall` retries it, which
//! is also what Homebrew does.

mod ca_bundle;
pub use ca_bundle::known_ca_bundle_script;
mod clang;
mod font_cache;
mod paths;
mod steps;
mod template;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use zb_core::{Guard, Platform, PostInstallStep};

/// What one formula's steps are resolved against.
#[derive(Debug, Clone)]
pub struct PostInstallContext {
    /// `HOMEBREW_PREFIX`.
    pub prefix: PathBuf,
    /// The Cellar.
    pub cellar: PathBuf,
    pub name: String,
    /// The formula's version without the revision suffix: `{{version}}`.
    pub version: String,
    /// The installed keg, `<cellar>/<name>/<version>[_<revision>]`.
    pub keg: PathBuf,
    /// Scratch space. Each `run` step gets a fresh `HOME` under it.
    pub temp: PathBuf,
    /// Where results worth keeping between installs go, such as the CA
    /// bundle. Survives uninstalls; `zb reset` removes it.
    pub cache: PathBuf,
}

/// Why a step did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    /// zerobrew does not know how to run this step.
    Unsupported(String),
    Failed(String),
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepError::Unsupported(reason) => f.write_str(reason),
            StepError::Failed(message) => f.write_str(message),
        }
    }
}

impl StepError {
    fn io(context: impl fmt::Display) -> impl FnOnce(std::io::Error) -> Self {
        move |err| StepError::Failed(format!("{context}: {err}"))
    }
}

/// The step that stopped a formula's post-install, with its position in
/// the list so the user can find it in the formula's JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    pub index: usize,
    pub kind: String,
    pub error: StepError,
}

impl fmt::Display for StepFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "step {} ({}): {}", self.index + 1, self.kind, self.error)
    }
}

/// Run `steps` in order. Returns the messages of any `warn` steps.
///
/// Guard results are remembered for the whole run, as Homebrew does: a
/// later step with the same guard sees the answer from before earlier
/// steps changed the file system.
pub fn run_steps(
    ctx: &PostInstallContext,
    steps: &[PostInstallStep],
) -> Result<Vec<String>, StepFailure> {
    let mut guard_results: HashMap<Guard, bool> = HashMap::new();
    let mut notices = Vec::new();

    for (index, step) in steps.iter().enumerate() {
        let failure = |error: StepError| StepFailure {
            index,
            kind: step.action.kind().to_string(),
            error,
        };

        let mut applies = true;
        for guard in &step.guards {
            let holds = match guard_results.get(guard) {
                Some(&cached) => cached,
                None => {
                    let holds = guard_holds(ctx, guard).map_err(failure)?;
                    guard_results.insert(guard.clone(), holds);
                    holds
                }
            };
            if !holds {
                applies = false;
                break;
            }
        }
        if !applies {
            continue;
        }

        if let Some(notice) = steps::run_step(ctx, &step.action).map_err(failure)? {
            notices.push(notice);
        }
    }

    Ok(notices)
}

fn guard_holds(ctx: &PostInstallContext, guard: &Guard) -> Result<bool, StepError> {
    Ok(match guard {
        Guard::IfExists(spec) => paths::any_exists(ctx, spec)?,
        Guard::UnlessExists(spec) => !paths::any_exists(ctx, spec)?,
        Guard::On(Platform::Macos) => cfg!(target_os = "macos"),
        Guard::On(Platform::Linux) => cfg!(target_os = "linux"),
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::PostInstallContext;
    use std::fs;
    use std::path::Path;

    /// A context rooted in `root`, with an empty keg for `name` created.
    pub(crate) fn context(root: &Path, name: &str, version: &str) -> PostInstallContext {
        let prefix = root.join("prefix");
        let cellar = prefix.join("Cellar");
        let keg = cellar.join(name).join(version);
        let temp = root.join("tmp");
        for dir in [&prefix, &keg, &temp] {
            fs::create_dir_all(dir).unwrap();
        }
        PostInstallContext {
            prefix,
            cellar,
            name: name.to_string(),
            version: version.split('_').next().unwrap().to_string(),
            keg,
            temp,
            cache: root.join("cache/post-install"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::context;
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use zb_core::{PathSpec, StepAction};

    fn mkdir_step(path: &str, guards: Vec<Guard>) -> PostInstallStep {
        PostInstallStep {
            guards,
            action: StepAction::MkdirP {
                path: PathSpec::absolute(path),
            },
        }
    }

    #[test]
    fn steps_run_in_order_and_notices_are_collected() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1.0");
        let dir = ctx.prefix.join("made");
        let steps = vec![
            mkdir_step(dir.to_str().unwrap(), vec![]),
            PostInstallStep {
                guards: vec![],
                action: StepAction::Warn {
                    message: "hello from {{formula_name}}".into(),
                },
            },
        ];
        let notices = run_steps(&ctx, &steps).unwrap();
        assert!(dir.is_dir());
        assert_eq!(notices, ["hello from x"]);
    }

    #[test]
    fn a_step_whose_guard_fails_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1.0");
        let dir = ctx.prefix.join("skipped");
        let missing = ctx.prefix.join("nope");
        let steps = vec![mkdir_step(
            dir.to_str().unwrap(),
            vec![Guard::IfExists(PathSpec::absolute(
                missing.to_str().unwrap(),
            ))],
        )];
        run_steps(&ctx, &steps).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn platform_guards_follow_the_build_target() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1.0");
        let here = ctx.prefix.join("here");
        let there = ctx.prefix.join("there");
        let (this, other) = if cfg!(target_os = "macos") {
            (Platform::Macos, Platform::Linux)
        } else {
            (Platform::Linux, Platform::Macos)
        };
        let steps = vec![
            mkdir_step(here.to_str().unwrap(), vec![Guard::On(this)]),
            mkdir_step(there.to_str().unwrap(), vec![Guard::On(other)]),
        ];
        run_steps(&ctx, &steps).unwrap();
        assert!(here.is_dir());
        assert!(!there.exists());
    }

    #[test]
    fn guard_results_are_remembered_across_steps() {
        // node: `remove` guarded by `if_exists npm`, then `copy` recreates
        // npm, then more steps guarded the same way must still see the
        // original answer.
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1.0");
        let probe = ctx.prefix.join("probe");
        let first = ctx.prefix.join("first");
        let second = ctx.prefix.join("second");
        let guard = Guard::UnlessExists(PathSpec::absolute(probe.to_str().unwrap()));
        let steps = vec![
            mkdir_step(first.to_str().unwrap(), vec![guard.clone()]),
            mkdir_step(probe.to_str().unwrap(), vec![]),
            mkdir_step(second.to_str().unwrap(), vec![guard]),
        ];
        run_steps(&ctx, &steps).unwrap();
        assert!(first.is_dir());
        assert!(second.is_dir(), "the cached guard result should still hold");
    }

    #[test]
    fn failure_names_the_step() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1.0");
        let steps = vec![
            mkdir_step(ctx.prefix.join("ok").to_str().unwrap(), vec![]),
            PostInstallStep {
                guards: vec![],
                action: StepAction::Unsupported {
                    kind: "bootstrap_pypy".into(),
                    reason: "step type `bootstrap_pypy` is not supported yet".into(),
                },
            },
        ];
        let failure = run_steps(&ctx, &steps).unwrap_err();
        assert_eq!(failure.index, 1);
        assert_eq!(failure.kind, "bootstrap_pypy");
        assert!(matches!(failure.error, StepError::Unsupported(_)));
        assert_eq!(
            failure.to_string(),
            "step 2 (bootstrap_pypy): step type `bootstrap_pypy` is not supported yet"
        );
    }

    #[test]
    fn a_failed_step_stops_the_run() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1.0");
        let file = ctx.prefix.join("file");
        fs::write(&file, "x").unwrap();
        let after = ctx.prefix.join("after");
        let steps = vec![
            PostInstallStep {
                guards: vec![],
                action: StepAction::Mkdir {
                    path: PathSpec::absolute(file.to_str().unwrap()),
                },
            },
            mkdir_step(after.to_str().unwrap(), vec![]),
        ];
        let failure = run_steps(&ctx, &steps).unwrap_err();
        assert_eq!(failure.index, 0);
        assert!(!after.exists());
    }
}
