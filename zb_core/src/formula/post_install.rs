//! Homebrew's declarative post-install steps.
//!
//! Since Homebrew 7 a formula's post-install work is published in the API
//! as `post_install_steps`: a list of small, typed operations with no Ruby
//! behind them. The schema is defined by `Library/Homebrew/install_steps.rb`
//! in Homebrew; the types here mirror the subset formulae actually use.
//!
//! Parsing is deliberately forgiving at the step level. A step whose type
//! is unknown, or whose fields do not match what we expect, becomes
//! [`StepAction::Unsupported`] instead of failing the whole formula, so a
//! new step type upstream can never stop a formula from installing. The
//! runner reports such steps instead of skipping them.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// Where a step path is anchored. Mirrors the `base:` keys Homebrew emits.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PathBase {
    /// An absolute path, or one with template tokens that expand to one.
    #[default]
    Absolute,
    /// Used verbatim, for symlink sources and commands looked up on `PATH`.
    Relative,
    /// The formula's keg, `Cellar/<name>/<version>`.
    Prefix,
    /// `<prefix>/opt/<name>`.
    OptPrefix,
    Bin,
    Sbin,
    Lib,
    Libexec,
    Share,
    /// `<keg>/share/<name>`.
    Pkgshare,
    /// `<prefix>/var`.
    Var,
    /// `<prefix>/etc`.
    Etc,
    /// `<prefix>/etc/<name>`.
    Pkgetc,
    /// `<keg>/Frameworks`.
    Frameworks,
    /// `Cellar/<name>`.
    Rack,
    HomebrewPrefix,
    Home,
    Temp,
    BashCompletion,
    ZshCompletion,
    FishCompletion,
    PwshCompletion,
    /// `<prefix>/etc/<other formula>`; needs [`PathSpec::formula`].
    FormulaPkgetc,
    /// `<prefix>/opt/<other formula>`; needs [`PathSpec::formula`].
    FormulaOptPrefix,
    /// Every directory on `PATH`. `path` is the deprecated spelling.
    #[serde(alias = "path")]
    SearchPath,
}

/// A path in a step: a (possibly templated, possibly globbed) string plus
/// the directory it is relative to.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct PathSpec {
    pub path: String,
    #[serde(default)]
    pub base: PathBase,
    /// The other formula for [`PathBase::FormulaPkgetc`] and
    /// [`PathBase::FormulaOptPrefix`].
    #[serde(default)]
    pub formula: Option<String>,
}

impl PathSpec {
    pub fn absolute(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            base: PathBase::Absolute,
            formula: None,
        }
    }

    pub fn based(base: PathBase, path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            base,
            formula: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Macos,
    Linux,
}

/// A condition on a step. Every guard must hold for the step to run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "RawGuard")]
pub enum Guard {
    IfExists(PathSpec),
    UnlessExists(PathSpec),
    On(Platform),
}

#[derive(Deserialize)]
struct RawGuard {
    condition: String,
    #[serde(default)]
    value: Option<Platform>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    base: PathBase,
    #[serde(default)]
    formula: Option<String>,
}

impl TryFrom<RawGuard> for Guard {
    type Error = String;

    fn try_from(raw: RawGuard) -> Result<Self, Self::Error> {
        let path_spec = || {
            raw.path
                .clone()
                .map(|path| PathSpec {
                    path,
                    base: raw.base,
                    formula: raw.formula.clone(),
                })
                .ok_or_else(|| format!("guard '{}' has no path", raw.condition))
        };
        match raw.condition.as_str() {
            "if_exists" => Ok(Guard::IfExists(path_spec()?)),
            "unless_exists" => Ok(Guard::UnlessExists(path_spec()?)),
            "on" => raw
                .value
                .map(Guard::On)
                .ok_or_else(|| "guard 'on' has no platform".to_string()),
            other => Err(format!("unknown guard condition '{other}'")),
        }
    }
}

/// Whether a step asks for elevated privileges. zerobrew never escalates,
/// so anything but [`Sudo::Never`] is reported as unsupported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sudo {
    #[default]
    Never,
    Always,
    IfNeeded,
}

impl<'de> Deserialize<'de> for Sudo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Bool(true) => Ok(Sudo::Always),
            Value::String(s) if s == "if_needed" => Ok(Sudo::IfNeeded),
            _ => Ok(Sudo::Never),
        }
    }
}

/// How `terminate_process` matches processes.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProcessMatch {
    /// `killall <name>`.
    #[default]
    Name,
    /// `pkill -f <name>`.
    Full,
}

fn one() -> u32 {
    1
}

/// One post-install operation. Field defaults follow Homebrew's runner.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepAction {
    Mkdir {
        path: PathSpec,
    },
    MkdirP {
        path: PathSpec,
    },
    Touch {
        path: PathSpec,
    },
    Move {
        source: PathSpec,
        target: PathSpec,
        #[serde(default)]
        force: bool,
        /// When present, decides what happens if the destination exists;
        /// when absent, `force` does.
        #[serde(default)]
        overwrite: Option<bool>,
        #[serde(default)]
        source_glob: bool,
    },
    #[serde(alias = "move_contents")]
    MoveChildren {
        source: PathSpec,
        target: PathSpec,
    },
    Copy {
        source: PathSpec,
        target: PathSpec,
        #[serde(default)]
        recursive: bool,
        /// Defaults to overwriting, unlike `move`.
        #[serde(default)]
        overwrite: Option<bool>,
        #[serde(default)]
        source_glob: bool,
    },
    Remove {
        paths: Vec<PathSpec>,
        #[serde(default)]
        recursive: bool,
        /// Only remove symlinks whose target contains this string.
        #[serde(default)]
        symlink_target_contains: Option<String>,
        /// Only remove files whose content contains this string.
        #[serde(default)]
        content_contains: Option<String>,
        #[serde(default)]
        sudo: Sudo,
    },
    Inreplace {
        path: PathSpec,
        before: String,
        after: String,
        #[serde(default)]
        regexp: bool,
        #[serde(default)]
        skip_audit: bool,
        #[serde(default)]
        first_only: bool,
    },
    LinkDir {
        source: PathSpec,
        target: PathSpec,
    },
    LinkChildren {
        source: PathSpec,
        target: PathSpec,
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        suffix: String,
    },
    Symlink {
        source: PathSpec,
        target: PathSpec,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        source_glob: bool,
        #[serde(default)]
        sudo: Sudo,
    },
    Write {
        path: PathSpec,
        content: String,
        #[serde(default)]
        overwrite: bool,
    },
    Run {
        command: PathSpec,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        stdin_path: Option<PathSpec>,
        #[serde(default)]
        stdout_path: Option<PathSpec>,
        #[serde(default)]
        chdir: Option<PathSpec>,
        #[serde(default)]
        allow_failure: bool,
        #[serde(default)]
        sudo: Sudo,
    },
    TerminateProcess {
        name: String,
        #[serde(default, rename = "match")]
        match_by: ProcessMatch,
        #[serde(default = "one")]
        attempts: u32,
        #[serde(default)]
        must_succeed: bool,
        #[serde(default)]
        failure_message: Option<String>,
        #[serde(default)]
        notices: Vec<String>,
        #[serde(default)]
        sudo: Sudo,
    },
    ChangeDylibId {
        source: PathSpec,
        id: String,
        #[serde(default)]
        resolve_source: bool,
    },
    Warn {
        message: String,
    },
    SetPermissions {
        paths: Vec<PathSpec>,
        /// A `chmod` mode such as `u+w` or `0755`.
        permissions: String,
        #[serde(default)]
        non_recursive: bool,
    },
    InstallGzippedExecutable {
        source: PathSpec,
        target: PathSpec,
    },
    /// Write clang's `-isysroot` config files under `etc/clang`. macOS only.
    ConfigureClangSystem,
    /// Generate GCC's specs file. Linux only.
    ConfigureGccRuntime,
    CompileGsettingsSchemas {
        path: PathSpec,
    },
    ConfigurePhp,
    GioQuerymodules {
        path: PathSpec,
    },
    GdkPixbufQueryLoaders,
    GtkUpdateIconCache {
        path: PathSpec,
    },
    UpdateMimeDatabase {
        path: PathSpec,
    },
    UpdateDesktopDatabase {
        path: PathSpec,
    },
    /// A step we do not know how to run. `kind` is Homebrew's type name.
    #[serde(skip)]
    Unsupported {
        kind: String,
        reason: String,
    },
}

/// Every step type the runner knows. A type outside this list is reported
/// as not supported yet; a type inside it with the wrong fields is a
/// malformed step, which is a different message.
const KNOWN_KINDS: &[&str] = &[
    "mkdir",
    "mkdir_p",
    "touch",
    "move",
    "move_children",
    "move_contents",
    "copy",
    "remove",
    "inreplace",
    "link_dir",
    "link_children",
    "symlink",
    "write",
    "run",
    "terminate_process",
    "change_dylib_id",
    "warn",
    "set_permissions",
    "install_gzipped_executable",
    "configure_clang_system",
    "configure_gcc_runtime",
    "configure_php",
    "compile_gsettings_schemas",
    "gio_querymodules",
    "gdk_pixbuf_query_loaders",
    "gtk_update_icon_cache",
    "update_mime_database",
    "update_desktop_database",
];

impl StepAction {
    /// Homebrew's name for this step type.
    pub fn kind(&self) -> &str {
        match self {
            StepAction::Mkdir { .. } => "mkdir",
            StepAction::MkdirP { .. } => "mkdir_p",
            StepAction::Touch { .. } => "touch",
            StepAction::Move { .. } => "move",
            StepAction::MoveChildren { .. } => "move_children",
            StepAction::Copy { .. } => "copy",
            StepAction::Remove { .. } => "remove",
            StepAction::Inreplace { .. } => "inreplace",
            StepAction::LinkDir { .. } => "link_dir",
            StepAction::LinkChildren { .. } => "link_children",
            StepAction::Symlink { .. } => "symlink",
            StepAction::Write { .. } => "write",
            StepAction::Run { .. } => "run",
            StepAction::TerminateProcess { .. } => "terminate_process",
            StepAction::ChangeDylibId { .. } => "change_dylib_id",
            StepAction::Warn { .. } => "warn",
            StepAction::SetPermissions { .. } => "set_permissions",
            StepAction::InstallGzippedExecutable { .. } => "install_gzipped_executable",
            StepAction::ConfigureClangSystem => "configure_clang_system",
            StepAction::ConfigureGccRuntime => "configure_gcc_runtime",
            StepAction::ConfigurePhp => "configure_php",
            StepAction::CompileGsettingsSchemas { .. } => "compile_gsettings_schemas",
            StepAction::GioQuerymodules { .. } => "gio_querymodules",
            StepAction::GdkPixbufQueryLoaders => "gdk_pixbuf_query_loaders",
            StepAction::GtkUpdateIconCache { .. } => "gtk_update_icon_cache",
            StepAction::UpdateMimeDatabase { .. } => "update_mime_database",
            StepAction::UpdateDesktopDatabase { .. } => "update_desktop_database",
            StepAction::Unsupported { kind, .. } => kind,
        }
    }
}

/// A post-install step: an action and the guards that gate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostInstallStep {
    pub guards: Vec<Guard>,
    pub action: StepAction,
}

impl PostInstallStep {
    fn unsupported(kind: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            guards: Vec::new(),
            action: StepAction::Unsupported {
                kind: kind.into(),
                reason: reason.into(),
            },
        }
    }
}

impl<'de> Deserialize<'de> for PostInstallStep {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Ok(Self::unsupported("<untyped>", "step has no type"));
        };
        let kind = kind.to_string();
        if !KNOWN_KINDS.contains(&kind.as_str()) {
            return Ok(Self::unsupported(
                kind.clone(),
                format!("step type `{kind}` is not supported yet"),
            ));
        }
        let guards = match value.get("guards") {
            None => Vec::new(),
            Some(raw) => match serde_json::from_value::<Vec<Guard>>(raw.clone()) {
                Ok(guards) => guards,
                Err(e) => {
                    return Ok(Self::unsupported(
                        kind.clone(),
                        format!("step `{kind}` has a bad guard: {e}"),
                    ));
                }
            },
        };
        let action = match serde_json::from_value::<StepAction>(value) {
            Ok(action) => action,
            Err(e) => StepAction::Unsupported {
                reason: format!("step `{kind}` has unexpected fields: {e}"),
                kind,
            },
        };
        Ok(Self { guards, action })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(json: &str) -> PostInstallStep {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn run_step_with_based_command_and_args() {
        let s = step(
            r#"{"command": {"base": "libexec", "path": "post-install"}, "type": "run",
                "args": ["{{pkgshare}}/cacert.pem", "{{pkgetc}}/cert.pem"]}"#,
        );
        assert!(s.guards.is_empty());
        match s.action {
            StepAction::Run {
                command,
                args,
                env,
                allow_failure,
                sudo,
                ..
            } => {
                assert_eq!(command, PathSpec::based(PathBase::Libexec, "post-install"));
                assert_eq!(args, ["{{pkgshare}}/cacert.pem", "{{pkgetc}}/cert.pem"]);
                assert!(env.is_empty());
                assert!(!allow_failure);
                assert_eq!(sudo, Sudo::Never);
            }
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn symlink_step_defaults() {
        let s = step(
            r#"{"source": {"path": "{{etc}}/ca-certificates/cert.pem"},
                "target": {"path": "{{pkgetc}}/cert.pem"}, "force": true, "type": "symlink"}"#,
        );
        assert_eq!(
            s.action,
            StepAction::Symlink {
                source: PathSpec::absolute("{{etc}}/ca-certificates/cert.pem"),
                target: PathSpec::absolute("{{pkgetc}}/cert.pem"),
                force: true,
                source_glob: false,
                sudo: Sudo::Never,
            }
        );
    }

    #[test]
    fn guards_are_parsed_and_removed_from_the_action() {
        let s = step(
            r#"{"paths": [{"path": "{{HOMEBREW_PREFIX}}/lib/node_modules/npm"}], "recursive": true,
                "guards": [{"path": "{{HOMEBREW_PREFIX}}/lib/node_modules/npm", "condition": "if_exists", "id": "1"}],
                "type": "remove"}"#,
        );
        assert_eq!(
            s.guards,
            vec![Guard::IfExists(PathSpec::absolute(
                "{{HOMEBREW_PREFIX}}/lib/node_modules/npm"
            ))]
        );
        assert!(matches!(
            s.action,
            StepAction::Remove {
                recursive: true,
                ..
            }
        ));
    }

    #[test]
    fn platform_guards() {
        let s = step(
            r#"{"paths": [{"base": "lib", "path": "python{{version.major_minor}}/venv/scripts/**/*"}],
                "permissions": "u+w", "non_recursive": true,
                "guards": [{"condition": "on", "value": "linux", "id": "2"}], "type": "set_permissions"}"#,
        );
        assert_eq!(s.guards, vec![Guard::On(Platform::Linux)]);
        match s.action {
            StepAction::SetPermissions {
                paths,
                permissions,
                non_recursive,
            } => {
                assert_eq!(paths[0].base, PathBase::Lib);
                assert_eq!(permissions, "u+w");
                assert!(non_recursive);
            }
            other => panic!("expected set_permissions, got {other:?}"),
        }
    }

    #[test]
    fn guard_with_base_and_unless_exists() {
        let s = step(
            r#"{"type": "touch", "path": {"base": "var", "path": "log/x"},
                "guards": [{"base": "homebrew_prefix", "path": "opt/ruby@3.4/lib/libruby.3.4.dylib",
                            "condition": "unless_exists", "id": "2"}]}"#,
        );
        assert_eq!(
            s.guards,
            vec![Guard::UnlessExists(PathSpec::based(
                PathBase::HomebrewPrefix,
                "opt/ruby@3.4/lib/libruby.3.4.dylib"
            ))]
        );
    }

    #[test]
    fn unknown_step_type_is_unsupported_not_an_error() {
        let s = step(r#"{"type": "bootstrap_cpython"}"#);
        assert_eq!(s.action.kind(), "bootstrap_cpython");
        match s.action {
            StepAction::Unsupported { reason, .. } => {
                assert_eq!(reason, "step type `bootstrap_cpython` is not supported yet");
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    #[test]
    fn malformed_known_step_is_unsupported_with_reason() {
        let s = step(r#"{"type": "symlink", "source": {"path": "a"}}"#);
        match s.action {
            StepAction::Unsupported { kind, reason } => {
                assert_eq!(kind, "symlink");
                assert!(
                    reason.starts_with("step `symlink` has unexpected fields"),
                    "{reason}"
                );
                assert!(reason.contains("target"), "{reason}");
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    #[test]
    fn unknown_base_is_unsupported() {
        let s = step(r#"{"type": "mkdir_p", "path": {"base": "appdir", "path": "x"}}"#);
        assert!(matches!(s.action, StepAction::Unsupported { .. }));
    }

    #[test]
    fn bad_guard_is_unsupported() {
        let s = step(
            r#"{"type": "mkdir_p", "path": {"path": "/x"},
                "guards": [{"condition": "if_exists", "id": "1"}]}"#,
        );
        match s.action {
            StepAction::Unsupported { reason, .. } => {
                assert!(
                    reason.starts_with("step `mkdir_p` has a bad guard"),
                    "{reason}"
                );
                assert!(reason.contains("no path"), "{reason}");
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    #[test]
    fn untyped_step_is_unsupported() {
        let s = step(r#"{"path": {"path": "/x"}}"#);
        assert_eq!(s.action.kind(), "<untyped>");
    }

    #[test]
    fn sudo_forms() {
        let yes = step(
            r#"{"type": "symlink", "source": {"path": "a"}, "target": {"path": "b"}, "sudo": true}"#,
        );
        let if_needed = step(
            r#"{"type": "symlink", "source": {"path": "a"}, "target": {"path": "b"}, "sudo": "if_needed"}"#,
        );
        let no = step(
            r#"{"type": "symlink", "source": {"path": "a"}, "target": {"path": "b"}, "sudo": false}"#,
        );
        let sudo_of = |s: PostInstallStep| match s.action {
            StepAction::Symlink { sudo, .. } => sudo,
            _ => unreachable!(),
        };
        assert_eq!(sudo_of(yes), Sudo::Always);
        assert_eq!(sudo_of(if_needed), Sudo::IfNeeded);
        assert_eq!(sudo_of(no), Sudo::Never);
    }

    #[test]
    fn terminate_process_defaults_and_match() {
        let s = step(r#"{"name": "gpg-agent", "type": "terminate_process"}"#);
        assert_eq!(
            s.action,
            StepAction::TerminateProcess {
                name: "gpg-agent".into(),
                match_by: ProcessMatch::Name,
                attempts: 1,
                must_succeed: false,
                failure_message: None,
                notices: Vec::new(),
                sudo: Sudo::Never,
            }
        );
        let full =
            step(r#"{"name": "x", "type": "terminate_process", "match": "full", "attempts": 3}"#);
        assert!(matches!(
            full.action,
            StepAction::TerminateProcess {
                match_by: ProcessMatch::Full,
                attempts: 3,
                ..
            }
        ));
    }

    #[test]
    fn move_contents_is_an_alias_for_move_children() {
        let s =
            step(r#"{"type": "move_contents", "source": {"path": "a"}, "target": {"path": "b"}}"#);
        assert_eq!(s.action.kind(), "move_children");
    }

    #[test]
    fn search_path_accepts_deprecated_spelling() {
        let s = step(r#"{"type": "touch", "path": {"base": "path", "path": "x"}}"#);
        match s.action {
            StepAction::Touch { path } => assert_eq!(path.base, PathBase::SearchPath),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inreplace_fields() {
        let s = step(
            r#"{"path": {"base": "etc", "path": "unbound/unbound.conf"}, "skip_audit": true,
                "type": "inreplace", "after": "username: \"{{user}}\"",
                "before": "username: \"@@HOMEBREW-UNBOUND-USER@@\""}"#,
        );
        assert_eq!(
            s.action,
            StepAction::Inreplace {
                path: PathSpec::based(PathBase::Etc, "unbound/unbound.conf"),
                before: "username: \"@@HOMEBREW-UNBOUND-USER@@\"".into(),
                after: "username: \"{{user}}\"".into(),
                regexp: false,
                skip_audit: true,
                first_only: false,
            }
        );
    }

    #[test]
    fn unit_steps_parse() {
        for kind in [
            "configure_clang_system",
            "configure_gcc_runtime",
            "gdk_pixbuf_query_loaders",
        ] {
            let s = step(&format!(r#"{{"type": "{kind}"}}"#));
            assert_eq!(s.action.kind(), kind);
            assert!(!matches!(s.action, StepAction::Unsupported { .. }));
        }
    }

    #[test]
    fn steps_list_on_formula() {
        let json = r#"{
            "name": "x", "versions": {"stable": "1.0"}, "dependencies": [],
            "bottle": {"stable": {"files": {}}},
            "post_install_steps": [
                {"type": "mkdir_p", "path": {"path": "{{HOMEBREW_PREFIX}}/lib/gio/modules"}},
                {"type": "something_new"}
            ]
        }"#;
        let formula: crate::Formula = serde_json::from_str(json).unwrap();
        assert_eq!(formula.post_install_steps.len(), 2);
        assert_eq!(formula.post_install_steps[0].action.kind(), "mkdir_p");
        assert_eq!(formula.post_install_steps[1].action.kind(), "something_new");

        let without: crate::Formula = serde_json::from_str(
            r#"{"name": "x", "versions": {"stable": "1.0"}, "dependencies": [],
                "bottle": {"stable": {"files": {}}}}"#,
        )
        .unwrap();
        assert!(without.post_install_steps.is_empty());
    }
}
