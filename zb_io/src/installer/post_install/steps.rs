//! Derived from Homebrew's `Library/Homebrew/install_steps.rb` and
//! `install_steps/formula_actions.rb` (https://github.com/Homebrew/brew).
//! Copyright (c) 2009-present, Homebrew contributors. BSD 2-Clause License,
//! see LICENSE-HOMEBREW.
//!
//! What each step does. The behaviour follows Homebrew's
//! `InstallSteps::Runner`, including its defaults and its quirks, so that a
//! prefix set up by either tool looks the same.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use regex::Regex;
use tracing::{debug, warn};
use zb_core::{PathSpec, ProcessMatch, StepAction, Sudo};

use super::ca_bundle::CaBundle;
use super::paths::{self, expand_glob, resolve};
use super::template::expand;
use super::{PostInstallContext, StepError, clang};

/// Lines of command output kept for an error message.
const OUTPUT_TAIL: usize = 20;

/// Run one step. Returns the message of a `warn` step.
pub(super) fn run_step(
    ctx: &PostInstallContext,
    action: &StepAction,
) -> Result<Option<String>, StepError> {
    match action {
        StepAction::Mkdir { path } => {
            let path = resolve(ctx, path)?;
            fs::create_dir(&path).map_err(StepError::io(format!("mkdir {}", path.display())))?;
        }
        StepAction::MkdirP { path } => {
            let path = resolve(ctx, path)?;
            fs::create_dir_all(&path)
                .map_err(StepError::io(format!("mkdir -p {}", path.display())))?;
        }
        StepAction::Touch { path } => touch(&resolve(ctx, path)?)?,
        StepAction::Move {
            source,
            target,
            force,
            overwrite,
            source_glob,
        } => move_path(ctx, source, target, *force, *overwrite, *source_glob)?,
        StepAction::MoveChildren { source, target } => move_children(ctx, source, target)?,
        StepAction::Copy {
            source,
            target,
            recursive,
            overwrite,
            source_glob,
        } => copy(
            ctx,
            source,
            target,
            *recursive,
            overwrite.unwrap_or(true),
            *source_glob,
        )?,
        StepAction::Remove {
            paths,
            recursive,
            symlink_target_contains,
            content_contains,
            sudo,
        } => {
            refuse_sudo(*sudo)?;
            remove(
                ctx,
                paths,
                *recursive,
                symlink_target_contains.as_deref(),
                content_contains.as_deref(),
            )?;
        }
        StepAction::Inreplace {
            path,
            before,
            after,
            regexp,
            skip_audit,
            first_only,
        } => inreplace(
            ctx,
            path,
            before,
            after,
            *regexp,
            !*skip_audit,
            !*first_only,
        )?,
        StepAction::LinkDir { source, target } => link_dir(ctx, source, target)?,
        StepAction::LinkChildren {
            source,
            target,
            prefix,
            suffix,
        } => link_children(ctx, source, target, prefix, suffix)?,
        StepAction::Symlink {
            source,
            target,
            force,
            source_glob,
            sudo,
        } => {
            refuse_sudo(*sudo)?;
            make_symlink(ctx, source, target, *force, *source_glob)?;
        }
        StepAction::Write {
            path,
            content,
            overwrite,
        } => {
            let path = resolve(ctx, path)?;
            if *overwrite || !path.exists() {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
                }
                write_atomically(&path, expand(ctx, content)?.as_bytes())?;
            }
        }
        StepAction::Run {
            command,
            args,
            env,
            stdin_path,
            stdout_path,
            chdir,
            allow_failure,
            sudo,
        } => {
            refuse_sudo(*sudo)?;
            let program = paths::command(ctx, command)?;
            let args = args
                .iter()
                .map(|arg| expand(ctx, arg))
                .collect::<Result<Vec<_>, _>>()?;
            let bundle = CaBundle::detect(ctx, &program, &args);
            if let Some(bundle) = &bundle
                && bundle.restore()?
            {
                debug!("restored the CA bundle from the cache");
            } else if let Some(bundle) = &bundle
                && bundle.build(ctx)?
            {
                debug!("built the CA bundle in process");
                bundle.store()?;
            } else {
                run_command(
                    ctx,
                    &program,
                    &args,
                    env,
                    stdin_path.as_ref(),
                    stdout_path.as_ref(),
                    chdir.as_ref(),
                    *allow_failure,
                )?;
                if let Some(bundle) = &bundle {
                    bundle.store()?;
                }
            }
        }
        StepAction::TerminateProcess {
            name,
            match_by,
            attempts,
            must_succeed,
            failure_message,
            notices,
            sudo,
        } => {
            refuse_sudo(*sudo)?;
            for notice in notices {
                debug!(notice = %expand(ctx, notice)?, "post-install notice");
            }
            terminate_process(
                ctx,
                name,
                *match_by,
                *attempts,
                *must_succeed,
                failure_message.as_deref(),
            )?;
        }
        StepAction::ChangeDylibId {
            source,
            id,
            resolve_source,
        } => change_dylib_id(ctx, source, id, *resolve_source)?,
        StepAction::Warn { message } => return Ok(Some(expand(ctx, message)?)),
        StepAction::SetPermissions {
            paths,
            permissions,
            non_recursive,
        } => set_permissions(ctx, paths, permissions, !*non_recursive)?,
        StepAction::InstallGzippedExecutable { source, target } => {
            install_gzipped_executable(&resolve(ctx, source)?, &resolve(ctx, target)?)?
        }
        StepAction::ConfigureClangSystem => clang::configure_clang_system(ctx)?,
        StepAction::ConfigureGccRuntime => {
            if cfg!(target_os = "linux") {
                return Err(StepError::Unsupported(
                    "configure_gcc_runtime is not implemented on Linux yet".to_string(),
                ));
            }
        }
        StepAction::CompileGsettingsSchemas { path } => run_formula_tool(
            ctx,
            "glib",
            "glib-compile-schemas",
            &[resolve(ctx, path)?.into()],
        )?,
        StepAction::GioQuerymodules { path } => run_formula_tool(
            ctx,
            "glib",
            "gio-querymodules",
            &[resolve(ctx, path)?.into()],
        )?,
        StepAction::GdkPixbufQueryLoaders => run_formula_tool(
            ctx,
            "gdk-pixbuf",
            "gdk-pixbuf-query-loaders",
            &["--update-cache".into()],
        )?,
        StepAction::GtkUpdateIconCache { path } => {
            let path: OsString = resolve(ctx, path)?.into();
            let args = ["-q".into(), "-t".into(), "-f".into(), path];
            if ctx.prefix.join("opt/gtk4").exists() {
                run_formula_tool(ctx, "gtk4", "gtk4-update-icon-cache", &args)?
            } else {
                run_formula_tool(ctx, "gtk+3", "gtk3-update-icon-cache", &args)?
            }
        }
        StepAction::UpdateMimeDatabase { path } => run_formula_tool(
            ctx,
            "shared-mime-info",
            "update-mime-database",
            &[resolve(ctx, path)?.into()],
        )?,
        StepAction::UpdateDesktopDatabase { path } => run_formula_tool(
            ctx,
            "desktop-file-utils",
            "update-desktop-database",
            &[resolve(ctx, path)?.into()],
        )?,
        StepAction::Unsupported { reason, .. } => {
            return Err(StepError::Unsupported(reason.clone()));
        }
    }
    Ok(None)
}

fn refuse_sudo(sudo: Sudo) -> Result<(), StepError> {
    match sudo {
        Sudo::Never => Ok(()),
        Sudo::Always | Sudo::IfNeeded => Err(StepError::Unsupported(
            "steps that need sudo are not supported".to_string(),
        )),
    }
}

/// Create a file, or update its modification time if it exists.
fn touch(path: &Path) -> Result<(), StepError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
    }
    let file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(StepError::io(format!("touch {}", path.display())))?;
    file.set_modified(std::time::SystemTime::now())
        .map_err(StepError::io(format!("touch {}", path.display())))
}

/// Write a file through a temporary neighbour so a reader never sees it
/// half written.
pub(super) fn write_atomically(path: &Path, content: &[u8]) -> Result<(), StepError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .map_err(StepError::io(format!("writing {}", path.display())))?;
    file.write_all(content)
        .map_err(StepError::io(format!("writing {}", path.display())))?;
    file.persist(path)
        .map(|_| ())
        .map_err(|e| StepError::Failed(format!("writing {}: {}", path.display(), e.error)))
}

/// The source of a `move` or `copy`: with `source_glob`, the glob must
/// match exactly one path.
fn single_source(
    ctx: &PostInstallContext,
    spec: &PathSpec,
    source_glob: bool,
) -> Result<PathBuf, StepError> {
    if !source_glob {
        return resolve(ctx, spec);
    }
    let mut sources: Vec<_> = expand_glob(ctx, spec)?
        .into_iter()
        .filter(|path| path.exists() || path.is_symlink())
        .collect();
    sources.dedup();
    if sources.len() != 1 {
        return Err(StepError::Failed(format!(
            "source glob must match exactly one path, matched {}: {}",
            sources.len(),
            spec.path
        )));
    }
    Ok(sources.remove(0))
}

/// Where a file lands: inside `target` if that is a directory, else at it.
fn destination(source: &Path, target: &Path) -> PathBuf {
    if target.is_dir() {
        match source.file_name() {
            Some(name) => target.join(name),
            None => target.to_path_buf(),
        }
    } else {
        target.to_path_buf()
    }
}

fn move_path(
    ctx: &PostInstallContext,
    source: &PathSpec,
    target: &PathSpec,
    force: bool,
    overwrite: Option<bool>,
    source_glob: bool,
) -> Result<(), StepError> {
    let source = single_source(ctx, source, source_glob)?;
    let target = resolve(ctx, target)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
    }
    let destination = destination(&source, &target);
    let may_overwrite = overwrite.map(|o| o || force).unwrap_or(force);
    if destination.exists() || destination.is_symlink() {
        if !may_overwrite {
            return Err(StepError::Failed(format!(
                "{} already exists",
                destination.display()
            )));
        }
        if destination != source {
            remove_any(&destination)?;
        }
    }
    fs::rename(&source, &destination).map_err(StepError::io(format!(
        "moving {} to {}",
        source.display(),
        destination.display()
    )))
}

fn move_children(
    ctx: &PostInstallContext,
    source: &PathSpec,
    target: &PathSpec,
) -> Result<(), StepError> {
    let source = resolve(ctx, source)?;
    let target = resolve(ctx, target)?;
    fs::create_dir_all(&target).map_err(StepError::io("creating directory"))?;
    for entry in
        fs::read_dir(&source).map_err(StepError::io(format!("reading {}", source.display())))?
    {
        let from = entry.map_err(StepError::io("reading directory"))?.path();
        if from == target {
            continue;
        }
        let to = target.join(from.file_name().unwrap_or_default());
        fs::rename(&from, &to).map_err(StepError::io(format!(
            "moving {} to {}",
            from.display(),
            to.display()
        )))?;
    }
    Ok(())
}

fn copy(
    ctx: &PostInstallContext,
    source: &PathSpec,
    target: &PathSpec,
    recursive: bool,
    overwrite: bool,
    source_glob: bool,
) -> Result<(), StepError> {
    let source = single_source(ctx, source, source_glob)?;
    let target = resolve(ctx, target)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
    }
    let destination = destination(&source, &target);
    if destination.exists() && !overwrite {
        return Err(StepError::Failed(format!(
            "{} already exists",
            destination.display()
        )));
    }
    if recursive {
        if overwrite {
            remove_any(&destination)?;
        }
        copy_tree(&source, &destination)
    } else {
        if overwrite && destination.is_symlink() {
            remove_any(&destination)?;
        }
        fs::copy(&source, &destination)
            .map(|_| ())
            .map_err(StepError::io(format!(
                "copying {} to {}",
                source.display(),
                destination.display()
            )))
    }
}

/// Copy a tree, keeping modes and recreating symlinks as symlinks.
fn copy_tree(source: &Path, destination: &Path) -> Result<(), StepError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(StepError::io(format!("reading {}", source.display())))?;
    let failed = |what: &str| StepError::io(format!("{what} {}", source.display()));
    if metadata.is_symlink() {
        let target = fs::read_link(source).map_err(failed("reading link"))?;
        symlink(target, destination).map_err(failed("copying link"))?;
    } else if metadata.is_dir() {
        fs::create_dir_all(destination).map_err(failed("copying directory"))?;
        for entry in fs::read_dir(source).map_err(failed("reading"))? {
            let entry = entry.map_err(failed("reading"))?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        fs::copy(source, destination).map_err(failed("copying"))?;
    }
    Ok(())
}

/// `rm -rf` for one path: files, symlinks and directories; missing is fine.
fn remove_any(path: &Path) -> Result<(), StepError> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(StepError::io(format!("removing {}", path.display())))
}

fn remove(
    ctx: &PostInstallContext,
    specs: &[PathSpec],
    recursive: bool,
    symlink_target_contains: Option<&str>,
    content_contains: Option<&str>,
) -> Result<(), StepError> {
    let mut paths = Vec::new();
    for spec in specs {
        paths.extend(expand_glob(ctx, spec)?);
    }
    if let Some(needle) = symlink_target_contains {
        let needle = expand(ctx, needle)?;
        paths.retain(|path| {
            path.is_symlink()
                && fs::read_link(path)
                    .map(|target| target.to_string_lossy().contains(&needle))
                    .unwrap_or(false)
        });
    }
    if let Some(needle) = content_contains {
        let needle = expand(ctx, needle)?;
        paths.retain(|path| {
            path.is_file()
                && fs::read(path)
                    .map(|content| String::from_utf8_lossy(&content).contains(&needle))
                    .unwrap_or(false)
        });
    }
    for path in paths {
        if recursive {
            remove_any(&path)?;
        } else if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.is_dir() {
                return Err(StepError::Failed(format!(
                    "{} is a directory; the step is not recursive",
                    path.display()
                )));
            }
            fs::remove_file(&path)
                .map_err(StepError::io(format!("removing {}", path.display())))?;
        }
    }
    Ok(())
}

fn inreplace(
    ctx: &PostInstallContext,
    path: &PathSpec,
    before: &str,
    after: &str,
    regexp: bool,
    audit: bool,
    global: bool,
) -> Result<(), StepError> {
    let path = resolve(ctx, path)?;
    let before = expand(ctx, before)?;
    let after = expand(ctx, after)?;
    let content =
        fs::read_to_string(&path).map_err(StepError::io(format!("reading {}", path.display())))?;

    let replaced = if regexp {
        // Ruby's `^` and `$` match at line boundaries.
        let pattern = Regex::new(&format!("(?m){before}"))
            .map_err(|e| StepError::Failed(format!("bad pattern '{before}': {e}")))?;
        let limit = if global { 0 } else { 1 };
        pattern
            .replacen(&content, limit, regex::NoExpand(&after))
            .into_owned()
    } else if global {
        content.replace(&before, &after)
    } else {
        content.replacen(&before, &after, 1)
    };

    if replaced == content {
        if audit {
            return Err(StepError::Failed(format!(
                "expected to replace '{before}' in {}, but it was not found",
                path.display()
            )));
        }
        return Ok(());
    }
    // Truncating an existing file keeps its mode.
    fs::write(&path, replaced).map_err(StepError::io(format!("writing {}", path.display())))
}

/// `source` relative to `base`, both absolute: `../lib/x` style.
fn relative_path(source: &Path, base: &Path) -> PathBuf {
    let mut source_parts = source.components().peekable();
    let mut base_parts = base.components().peekable();
    while let (Some(s), Some(b)) = (source_parts.peek(), base_parts.peek()) {
        if s != b {
            break;
        }
        source_parts.next();
        base_parts.next();
    }
    let mut relative = PathBuf::new();
    for part in base_parts {
        if !matches!(part, Component::RootDir | Component::CurDir) {
            relative.push("..");
        }
    }
    relative.extend(source_parts);
    relative
}

/// Homebrew's `install_symlink`: a relative link at `link`, pointing at
/// `source` from the real location of `link`'s directory.
fn install_symlink(source: &Path, link: &Path) -> Result<(), StepError> {
    let dir = link.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir).map_err(StepError::io("creating directory"))?;
    let dir = dir
        .canonicalize()
        .map_err(StepError::io(format!("resolving {}", dir.display())))?;
    let source = match (source.parent(), source.file_name()) {
        (Some(parent), Some(name)) if parent.exists() => parent
            .canonicalize()
            .map_err(StepError::io(format!("resolving {}", parent.display())))?
            .join(name),
        _ => source.to_path_buf(),
    };
    let link = dir.join(link.file_name().unwrap_or_default());
    remove_any(&link)?;
    symlink(relative_path(&source, &dir), &link)
        .map_err(StepError::io(format!("linking {}", link.display())))
}

fn link_dir(
    ctx: &PostInstallContext,
    source: &PathSpec,
    target: &PathSpec,
) -> Result<(), StepError> {
    let source_dir = resolve(ctx, source)?;
    let target_dir = resolve(ctx, target)?;
    for entry in walkdir::WalkDir::new(&source_dir) {
        let entry = entry
            .map_err(|e| StepError::Failed(format!("reading {}: {e}", source_dir.display())))?;
        let source = entry.path();
        if source.file_name().is_some_and(|name| name == ".DS_Store") {
            continue;
        }
        let relative = source.strip_prefix(&source_dir).unwrap_or(source);
        let link = target_dir.join(relative);
        if link.is_dir() && !link.is_symlink() {
            continue;
        }
        remove_any(&link)?;
        let file_type = entry.file_type();
        if file_type.is_symlink() || file_type.is_file() {
            install_symlink(source, &link)?;
        } else if file_type.is_dir() {
            fs::create_dir_all(&link).map_err(StepError::io("creating directory"))?;
        }
    }
    Ok(())
}

fn link_children(
    ctx: &PostInstallContext,
    source: &PathSpec,
    target: &PathSpec,
    prefix: &str,
    suffix: &str,
) -> Result<(), StepError> {
    let source_dir = resolve(ctx, source)?;
    let target_dir = resolve(ctx, target)?;
    fs::create_dir_all(&target_dir).map_err(StepError::io("creating directory"))?;
    let prefix = expand(ctx, prefix)?;
    let suffix = expand(ctx, suffix)?;
    for entry in fs::read_dir(&source_dir)
        .map_err(StepError::io(format!("reading {}", source_dir.display())))?
    {
        let child = entry.map_err(StepError::io("reading directory"))?.path();
        let name = child.file_name().unwrap_or_default().to_string_lossy();
        install_symlink(&child, &target_dir.join(format!("{prefix}{name}{suffix}")))?;
    }
    Ok(())
}

fn make_symlink(
    ctx: &PostInstallContext,
    source: &PathSpec,
    target: &PathSpec,
    force: bool,
    source_glob: bool,
) -> Result<(), StepError> {
    let target = resolve(ctx, target)?;
    if !source_glob {
        return create_symlink(&paths::link_target(ctx, source)?, &target, force);
    }
    let sources = expand_glob(ctx, source)?;
    match sources.as_slice() {
        [] => Ok(()),
        [single] if !target.is_dir() => create_symlink(single, &target, force),
        _ => {
            fs::create_dir_all(&target).map_err(StepError::io("creating directory"))?;
            for source in &sources {
                let link = target.join(source.file_name().unwrap_or_default());
                create_symlink(source, &link, force)?;
            }
            Ok(())
        }
    }
}

/// An absolute (or verbatim) symlink, as Homebrew's `symlink` step makes.
fn create_symlink(source: &Path, link: &Path, force: bool) -> Result<(), StepError> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
    }
    if force && (link.is_symlink() || link.is_file()) {
        fs::remove_file(link).map_err(StepError::io(format!("removing {}", link.display())))?;
    }
    symlink(source, link).map_err(StepError::io(format!("linking {}", link.display())))
}

/// The `PATH` a step command runs with: the prefix first, then the system's.
pub(super) fn command_path(ctx: &PostInstallContext) -> String {
    let system_path = std::env::var("PATH").unwrap_or_default();
    format!(
        "{}:{}:{system_path}",
        ctx.prefix.join("bin").display(),
        ctx.prefix.join("sbin").display()
    )
}

/// The environment a step command runs with: the prefix first on `PATH`,
/// a private `HOME`, and the C locale, so that what a step writes does not
/// depend on the user's language settings. ca-certificates' script, for
/// one, orders its bundle by a glob, which bash sorts by locale.
pub(super) fn command_env(ctx: &PostInstallContext, home: &Path) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), command_path(ctx)),
        ("LC_ALL".into(), "C".into()),
        ("HOME".into(), home.display().to_string()),
        ("HOMEBREW_PREFIX".into(), ctx.prefix.display().to_string()),
        ("HOMEBREW_CELLAR".into(), ctx.cellar.display().to_string()),
        ("ZEROBREW_PREFIX".into(), ctx.prefix.display().to_string()),
    ]
}

fn output_tail(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(OUTPUT_TAIL);
    lines[start..].join("\n")
}

#[allow(clippy::too_many_arguments)]
fn run_command(
    ctx: &PostInstallContext,
    program: &OsStr,
    args: &[String],
    env: &BTreeMap<String, String>,
    stdin_path: Option<&PathSpec>,
    stdout_path: Option<&PathSpec>,
    chdir: Option<&PathSpec>,
    allow_failure: bool,
) -> Result<(), StepError> {
    let home = tempfile::Builder::new()
        .prefix(&format!("{}-postinstall-", ctx.name))
        .tempdir_in(&ctx.temp)
        .map_err(StepError::io("creating a temporary home"))?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(command_env(ctx, home.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, expand(ctx, value)?);
    }
    if let Some(dir) = chdir {
        cmd.current_dir(resolve(ctx, dir)?);
    }
    let input = match stdin_path {
        Some(spec) => {
            let path = resolve(ctx, spec)?;
            Some(fs::read(&path).map_err(StepError::io(format!("reading {}", path.display())))?)
        }
        None => None,
    };
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let shown = format!("{} {}", program.to_string_lossy(), args.join(" "));
    debug!(command = %shown, "running post-install command");
    let mut child = cmd
        .spawn()
        .map_err(StepError::io(format!("running {shown}")))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // A command that exits early closes its stdin; that is not an error.
        let _ = stdin.write_all(&input);
    }
    let output = child
        .wait_with_output()
        .map_err(StepError::io(format!("waiting for {shown}")))?;

    if !output.status.success() {
        if allow_failure {
            warn!(command = %shown, status = %output.status, "post-install command failed");
            return Ok(());
        }
        let tail = output_tail(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        let mut message = format!("`{shown}` failed with {}", output.status);
        if !tail.is_empty() {
            message.push('\n');
            message.push_str(&tail);
        }
        return Err(StepError::Failed(message));
    }

    if let Some(spec) = stdout_path {
        let path = resolve(ctx, spec)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
        }
        fs::write(&path, &output.stdout)
            .map_err(StepError::io(format!("writing {}", path.display())))?;
    }
    Ok(())
}

fn terminate_process(
    ctx: &PostInstallContext,
    name: &str,
    match_by: ProcessMatch,
    attempts: u32,
    must_succeed: bool,
    failure_message: Option<&str>,
) -> Result<(), StepError> {
    let name = expand(ctx, name)?;
    let (program, args): (&str, Vec<&str>) = match match_by {
        ProcessMatch::Name => ("/usr/bin/killall", vec![&name]),
        ProcessMatch::Full => ("/usr/bin/pkill", vec!["-f", &name]),
    };
    let mut remaining = attempts.max(1);
    loop {
        let status = Command::new(program)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(status, Ok(status) if status.success()) {
            return Ok(());
        }
        remaining -= 1;
        if remaining == 0 {
            if let Some(message) = failure_message {
                warn!(message = %expand(ctx, message)?, "post-install");
            }
            if must_succeed {
                return Err(StepError::Failed(format!("could not terminate {name}")));
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(target_os = "macos")]
fn change_dylib_id(
    ctx: &PostInstallContext,
    source: &PathSpec,
    id: &str,
    resolve_source: bool,
) -> Result<(), StepError> {
    let mut path = resolve(ctx, source)?;
    if resolve_source {
        path = path
            .canonicalize()
            .map_err(StepError::io(format!("resolving {}", path.display())))?;
    }
    let id = expand(ctx, id)?;
    crate::extraction::patch::macos::set_install_name(&path, &id)
        .map_err(|e| StepError::Failed(e.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn change_dylib_id(
    _ctx: &PostInstallContext,
    _source: &PathSpec,
    _id: &str,
    _resolve_source: bool,
) -> Result<(), StepError> {
    Err(StepError::Unsupported(
        "change_dylib_id only applies to Mach-O files".to_string(),
    ))
}

fn set_permissions(
    ctx: &PostInstallContext,
    specs: &[PathSpec],
    permissions: &str,
    recursive: bool,
) -> Result<(), StepError> {
    let mut paths = Vec::new();
    for spec in specs {
        paths.extend(
            expand_glob(ctx, spec)?
                .into_iter()
                .filter(|path| path.exists()),
        );
    }
    if paths.is_empty() {
        return Ok(());
    }
    // `chmod` parses symbolic modes like `u+w`; no point reimplementing it.
    let mut cmd = Command::new("chmod");
    if recursive {
        cmd.arg("-R");
    }
    let output = cmd
        .arg("--")
        .arg(permissions)
        .args(&paths)
        .output()
        .map_err(StepError::io("running chmod"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(StepError::Failed(format!(
            "chmod {permissions} failed: {}",
            output_tail(&output.stderr)
        )))
    }
}

fn install_gzipped_executable(source: &Path, target: &Path) -> Result<(), StepError> {
    if !source.exists() {
        return Ok(());
    }
    let failed = |what: &str| StepError::io(format!("{what} {}", target.display()));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(failed("creating directory for"))?;
    }
    let compressed =
        fs::File::open(source).map_err(StepError::io(format!("opening {}", source.display())))?;
    let mut content = Vec::new();
    flate2::read::GzDecoder::new(compressed)
        .read_to_end(&mut content)
        .map_err(StepError::io(format!("decompressing {}", source.display())))?;
    write_atomically(target, &content)?;
    fs::set_permissions(target, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(failed("setting mode of"))?;
    fs::remove_file(source).map_err(StepError::io(format!("removing {}", source.display())))
}

/// Run a tool from another formula's `opt` prefix, as `gtk_update_icon_cache`
/// and friends do.
fn run_formula_tool(
    ctx: &PostInstallContext,
    formula: &str,
    executable: &str,
    args: &[OsString],
) -> Result<(), StepError> {
    let tool = ctx
        .prefix
        .join("opt")
        .join(formula)
        .join("bin")
        .join(executable);
    if !tool.is_file() {
        return Err(StepError::Failed(format!(
            "{formula} is missing required executable: {}",
            tool.display()
        )));
    }
    let home = tempfile::Builder::new()
        .prefix(&format!("{}-postinstall-", ctx.name))
        .tempdir_in(&ctx.temp)
        .map_err(StepError::io("creating a temporary home"))?;
    let output = Command::new(&tool)
        .args(args)
        .envs(command_env(ctx, home.path()))
        .output()
        .map_err(StepError::io(format!("running {}", tool.display())))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(StepError::Failed(format!(
            "`{}` failed with {}\n{}",
            tool.display(),
            output.status,
            output_tail(&output.stderr)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::context;
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use zb_core::PathBase;

    fn run(ctx: &PostInstallContext, action: StepAction) -> Result<Option<String>, StepError> {
        run_step(ctx, &action)
    }

    fn abs(path: &Path) -> PathSpec {
        PathSpec::absolute(path.to_str().unwrap())
    }

    #[test]
    fn mkdir_fails_when_present_and_mkdir_p_does_not() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let dir = ctx.prefix.join("a/b");
        assert!(run(&ctx, StepAction::Mkdir { path: abs(&dir) }).is_err());
        run(&ctx, StepAction::MkdirP { path: abs(&dir) }).unwrap();
        assert!(dir.is_dir());
        run(&ctx, StepAction::MkdirP { path: abs(&dir) }).unwrap();
        assert!(run(&ctx, StepAction::Mkdir { path: abs(&dir) }).is_err());
    }

    #[test]
    fn touch_creates_parents_and_file() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "gnupg", "2.4");
        run(
            &ctx,
            StepAction::Touch {
                path: PathSpec::based(PathBase::Var, "run/gnupg/.keep"),
            },
        )
        .unwrap();
        assert!(ctx.prefix.join("var/run/gnupg/.keep").is_file());
    }

    #[test]
    fn write_respects_overwrite_and_expands_tokens() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "node@18", "18.0");
        let path = ctx.keg.join("lib/node_modules/npm/npmrc");
        let write = |overwrite| StepAction::Write {
            path: abs(&path),
            content: "prefix = {{HOMEBREW_PREFIX}}\n".into(),
            overwrite,
        };
        run(&ctx, write(false)).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("prefix = {}\n", ctx.prefix.display())
        );
        fs::write(&path, "edited").unwrap();
        run(&ctx, write(false)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "edited");
        run(&ctx, write(true)).unwrap();
        assert!(fs::read_to_string(&path).unwrap().starts_with("prefix = "));
    }

    #[test]
    fn symlink_force_replaces_and_plain_fails_on_existing() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "openssl@3", "3.6");
        let cert = ctx.prefix.join("etc/ca-certificates/cert.pem");
        fs::create_dir_all(cert.parent().unwrap()).unwrap();
        fs::write(&cert, "certs").unwrap();
        let link = ctx.prefix.join("etc/openssl@3/cert.pem");
        let step = |force| StepAction::Symlink {
            source: PathSpec::absolute("{{etc}}/ca-certificates/cert.pem"),
            target: PathSpec::absolute("{{pkgetc}}/cert.pem"),
            force,
            source_glob: false,
            sudo: Sudo::Never,
        };
        run(&ctx, step(true)).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), cert);
        assert!(run(&ctx, step(false)).is_err());
        run(&ctx, step(true)).unwrap();
        assert_eq!(fs::read_to_string(&link).unwrap(), "certs");
    }

    #[test]
    fn symlink_with_glob_links_each_match_into_the_target() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "node", "25");
        let man = ctx.prefix.join("lib/node_modules/npm/man/man1");
        fs::create_dir_all(&man).unwrap();
        for name in ["npm.1", "npx.1", "other.1"] {
            fs::write(man.join(name), "").unwrap();
        }
        let target = ctx.prefix.join("share/man/man1");
        run(
            &ctx,
            StepAction::Symlink {
                source: PathSpec::absolute(
                    "{{HOMEBREW_PREFIX}}/lib/node_modules/npm/man/man1/{npm,npx,package-}*",
                ),
                target: abs(&target),
                force: true,
                source_glob: true,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_link(target.join("npm.1")).unwrap(),
            man.join("npm.1")
        );
        assert!(target.join("npx.1").is_symlink());
        assert!(!target.join("other.1").exists());

        // No match: nothing happens, no error.
        run(
            &ctx,
            StepAction::Symlink {
                source: PathSpec::absolute("{{HOMEBREW_PREFIX}}/nowhere/*"),
                target: abs(&target),
                force: true,
                source_glob: true,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
    }

    #[test]
    fn symlink_with_relative_source_is_verbatim() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let link = ctx.keg.join("bin/x");
        run(
            &ctx,
            StepAction::Symlink {
                source: PathSpec::based(PathBase::Relative, "../libexec/x"),
                target: abs(&link),
                force: false,
                source_glob: false,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("../libexec/x"));
    }

    #[test]
    fn sudo_steps_are_refused() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let err = run(
            &ctx,
            StepAction::Symlink {
                source: PathSpec::absolute("/a"),
                target: abs(&ctx.prefix.join("b")),
                force: false,
                source_glob: false,
                sudo: Sudo::Always,
            },
        )
        .unwrap_err();
        assert!(matches!(err, StepError::Unsupported(_)));
    }

    #[test]
    fn copy_recursive_replaces_the_destination_tree() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "node", "25");
        let npm = ctx.keg.join("libexec/lib/node_modules/npm");
        fs::create_dir_all(npm.join("bin")).unwrap();
        fs::write(npm.join("bin/npm-cli.js"), "#!js").unwrap();
        fs::set_permissions(
            npm.join("bin/npm-cli.js"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("bin/npm-cli.js", npm.join("link")).unwrap();
        let modules = ctx.prefix.join("lib/node_modules");
        fs::create_dir_all(modules.join("npm/stale")).unwrap();

        run(
            &ctx,
            StepAction::Copy {
                source: PathSpec::absolute("{{libexec}}/lib/node_modules/npm"),
                target: abs(&modules),
                recursive: true,
                overwrite: None,
                source_glob: false,
            },
        )
        .unwrap();
        let copied = modules.join("npm");
        assert!(copied.join("bin/npm-cli.js").is_file());
        assert_eq!(
            fs::metadata(copied.join("bin/npm-cli.js"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(copied.join("link")).unwrap(),
            PathBuf::from("bin/npm-cli.js")
        );
        assert!(
            !copied.join("stale").exists(),
            "overwrite removes the old tree"
        );
    }

    #[test]
    fn copy_file_into_directory_and_refuses_without_overwrite() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let src = ctx.keg.join("a.txt");
        fs::write(&src, "a").unwrap();
        let dir = ctx.prefix.join("dir");
        fs::create_dir_all(&dir).unwrap();
        let step = |overwrite| StepAction::Copy {
            source: abs(&src),
            target: abs(&dir),
            recursive: false,
            overwrite,
            source_glob: false,
        };
        run(&ctx, step(None)).unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "a");
        assert!(run(&ctx, step(Some(false))).is_err());
        run(&ctx, step(Some(true))).unwrap();
    }

    #[test]
    fn move_semantics() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let a = ctx.keg.join("a");
        let b = ctx.keg.join("b");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let step = |force, overwrite| StepAction::Move {
            source: abs(&a),
            target: abs(&b),
            force,
            overwrite,
            source_glob: false,
        };
        assert!(run(&ctx, step(false, None)).is_err());
        assert!(run(&ctx, step(false, Some(false))).is_err());
        run(&ctx, step(true, None)).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read_to_string(&b).unwrap(), "a");
    }

    #[test]
    fn move_children_moves_everything_but_the_target() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let src = ctx.keg.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("f"), "").unwrap();
        let dst = src.join("into");
        run(
            &ctx,
            StepAction::MoveChildren {
                source: abs(&src),
                target: abs(&dst),
            },
        )
        .unwrap();
        assert!(dst.join("f").is_file());
        assert!(dst.join("sub").is_dir());
        assert!(!src.join("f").exists());
    }

    #[test]
    fn remove_with_filters_and_recursion() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ruby", "4.0.1");
        let gems = ctx.prefix.join("lib/ruby/gems/4.0.0/gems");
        fs::create_dir_all(gems.join("bundler-2.6/lib")).unwrap();
        fs::create_dir_all(gems.join("rake-13")).unwrap();
        run(
            &ctx,
            StepAction::Remove {
                paths: vec![PathSpec::absolute(
                    "{{HOMEBREW_PREFIX}}/lib/ruby/gems/{{version.major_minor}}.0/gems/bundler-*",
                )],
                recursive: true,
                symlink_target_contains: None,
                content_contains: None,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
        assert!(!gems.join("bundler-2.6").exists());
        assert!(gems.join("rake-13").is_dir());

        let dir = ctx.prefix.join("links");
        fs::create_dir_all(&dir).unwrap();
        symlink("/opt/homebrew/old", dir.join("stale")).unwrap();
        symlink("/elsewhere", dir.join("keep")).unwrap();
        fs::write(dir.join("marker"), "generated by homebrew").unwrap();
        fs::write(dir.join("mine"), "hand written").unwrap();
        run(
            &ctx,
            StepAction::Remove {
                paths: vec![PathSpec::absolute(format!("{}/*", dir.display()))],
                recursive: false,
                symlink_target_contains: Some("/opt/homebrew".into()),
                content_contains: None,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
        assert!(!dir.join("stale").is_symlink());
        assert!(dir.join("keep").is_symlink());
        run(
            &ctx,
            StepAction::Remove {
                paths: vec![PathSpec::absolute(format!("{}/*", dir.display()))],
                recursive: false,
                symlink_target_contains: None,
                content_contains: Some("homebrew".into()),
                sudo: Sudo::Never,
            },
        )
        .unwrap();
        assert!(!dir.join("marker").exists());
        assert!(dir.join("mine").exists());

        // A directory cannot be removed without `recursive`.
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(
            run(
                &ctx,
                StepAction::Remove {
                    paths: vec![abs(&sub)],
                    recursive: false,
                    symlink_target_contains: None,
                    content_contains: None,
                    sudo: Sudo::Never,
                },
            )
            .is_err()
        );
        // A missing path is fine.
        run(
            &ctx,
            StepAction::Remove {
                paths: vec![abs(&dir.join("nothing"))],
                recursive: false,
                symlink_target_contains: None,
                content_contains: None,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
    }

    #[test]
    fn inreplace_literal_regex_audit_and_mode() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "unbound", "1.2");
        let conf = ctx.prefix.join("etc/unbound/unbound.conf");
        fs::create_dir_all(conf.parent().unwrap()).unwrap();
        fs::write(
            &conf,
            "server:\n  username: \"@@HOMEBREW-UNBOUND-USER@@\"\n",
        )
        .unwrap();
        fs::set_permissions(&conf, fs::Permissions::from_mode(0o600)).unwrap();
        let literal = |before: &str, skip_audit| StepAction::Inreplace {
            path: PathSpec::based(PathBase::Etc, "unbound/unbound.conf"),
            before: before.into(),
            after: "username: \"{{formula_name}}\"".into(),
            regexp: false,
            skip_audit,
            first_only: false,
        };
        run(
            &ctx,
            literal("username: \"@@HOMEBREW-UNBOUND-USER@@\"", true),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&conf).unwrap(),
            "server:\n  username: \"unbound\"\n"
        );
        assert_eq!(
            fs::metadata(&conf).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(run(&ctx, literal("not there", false)).is_err());
        run(&ctx, literal("not there", true)).unwrap();

        fs::write(&conf, "nagios_user=old\nother=1\nnagios_user=again\n").unwrap();
        run(
            &ctx,
            StepAction::Inreplace {
                path: abs(&conf),
                before: "^nagios_user=.*".into(),
                after: "nagios_user={{formula_name}}".into(),
                regexp: true,
                skip_audit: false,
                first_only: false,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&conf).unwrap(),
            "nagios_user=unbound\nother=1\nnagios_user=unbound\n"
        );
        fs::write(&conf, "a a a").unwrap();
        run(
            &ctx,
            StepAction::Inreplace {
                path: abs(&conf),
                before: "a".into(),
                after: "b".into(),
                regexp: false,
                skip_audit: false,
                first_only: true,
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&conf).unwrap(), "b a a");
    }

    #[test]
    fn link_children_makes_relative_links_with_affixes() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let src = ctx.keg.join("libexec/bin");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("tool"), "").unwrap();
        let dst = ctx.prefix.join("bin");
        run(
            &ctx,
            StepAction::LinkChildren {
                source: abs(&src),
                target: abs(&dst),
                prefix: "".into(),
                suffix: "{{version.major}}".into(),
            },
        )
        .unwrap();
        let link = dst.join("tool1");
        let target = fs::read_link(&link).unwrap();
        assert!(target.is_relative(), "{}", target.display());
        assert_eq!(
            link.parent().unwrap().join(&target).canonicalize().unwrap(),
            src.join("tool").canonicalize().unwrap()
        );
    }

    #[test]
    fn link_dir_mirrors_a_tree_with_links_and_keeps_real_directories() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let src = ctx.keg.join("share/tree");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a"), "").unwrap();
        fs::write(src.join("sub/b"), "").unwrap();
        fs::write(src.join(".DS_Store"), "").unwrap();
        let dst = ctx.prefix.join("share/tree");
        fs::create_dir_all(dst.join("sub")).unwrap();
        fs::write(dst.join("a"), "old").unwrap();
        run(
            &ctx,
            StepAction::LinkDir {
                source: abs(&src),
                target: abs(&dst),
            },
        )
        .unwrap();
        assert!(dst.join("a").is_symlink());
        assert!(dst.join("sub").is_dir() && !dst.join("sub").is_symlink());
        assert!(dst.join("sub/b").is_symlink());
        assert!(!dst.join(".DS_Store").exists());
    }

    #[test]
    fn set_permissions_applies_symbolic_modes_to_existing_matches() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "python@3.14", "3.14.8");
        let scripts = ctx.keg.join("lib/python3.14/venv/scripts/common");
        fs::create_dir_all(&scripts).unwrap();
        let file = scripts.join("activate");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        run(
            &ctx,
            StepAction::SetPermissions {
                paths: vec![PathSpec::based(
                    PathBase::Lib,
                    "python{{version.major_minor}}/venv/scripts/**/*",
                )],
                permissions: "u+w".into(),
                non_recursive: true,
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o644
        );
        // Nothing matching is not an error.
        run(
            &ctx,
            StepAction::SetPermissions {
                paths: vec![PathSpec::based(PathBase::Frameworks, "none/**/*")],
                permissions: "u+w".into(),
                non_recursive: true,
            },
        )
        .unwrap();
    }

    #[test]
    fn run_executes_with_expanded_args_env_stdin_and_stdout() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-10-01");
        let script = ctx.keg.join("libexec/post-install");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(
            &script,
            "#!/bin/sh\nmkdir -p \"$(dirname \"$2\")\"\ncat \"$1\" > \"$2\"\necho \"home=$HOME env=$EXTRA\" \n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let bundle = ctx.keg.join("share/ca-certificates/cacert.pem");
        fs::create_dir_all(bundle.parent().unwrap()).unwrap();
        fs::write(&bundle, "CERTS").unwrap();
        let log = ctx.prefix.join("var/log/post-install.log");

        run(
            &ctx,
            StepAction::Run {
                command: PathSpec::based(PathBase::Libexec, "post-install"),
                args: vec![
                    "{{pkgshare}}/cacert.pem".into(),
                    "{{pkgetc}}/cert.pem".into(),
                ],
                env: BTreeMap::from([("EXTRA".to_string(), "{{formula_name}}".to_string())]),
                stdin_path: None,
                stdout_path: Some(abs(&log)),
                chdir: None,
                allow_failure: false,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(ctx.prefix.join("etc/ca-certificates/cert.pem")).unwrap(),
            "CERTS"
        );
        let out = fs::read_to_string(&log).unwrap();
        assert!(out.contains("env=ca-certificates"), "{out}");
        assert!(
            out.contains(&format!("home={}", ctx.temp.display())),
            "{out}"
        );
        assert!(
            fs::read_dir(&ctx.temp).unwrap().next().is_none(),
            "temporary home is removed"
        );

        // stdin comes from a file; a bare command is found on PATH.
        let words = ctx.prefix.join("var/words");
        fs::create_dir_all(words.parent().unwrap()).unwrap();
        fs::write(&words, "one\ntwo\n").unwrap();
        let counted = ctx.prefix.join("var/count");
        run(
            &ctx,
            StepAction::Run {
                command: PathSpec::absolute("wc"),
                args: vec!["-l".into()],
                env: BTreeMap::new(),
                stdin_path: Some(abs(&words)),
                stdout_path: Some(abs(&counted)),
                chdir: Some(abs(&ctx.prefix)),
                allow_failure: false,
                sudo: Sudo::Never,
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&counted).unwrap().trim(), "2");
    }

    #[test]
    fn run_failure_reports_the_command_and_output() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let step = |allow_failure| StepAction::Run {
            command: PathSpec::absolute("sh"),
            args: vec!["-c".into(), "echo boom >&2; exit 3".into()],
            env: BTreeMap::new(),
            stdin_path: None,
            stdout_path: None,
            chdir: None,
            allow_failure,
            sudo: Sudo::Never,
        };
        let err = run(&ctx, step(false)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("exit status: 3"), "{message}");
        assert!(message.contains("boom"), "{message}");
        run(&ctx, step(true)).unwrap();

        let missing = run(
            &ctx,
            StepAction::Run {
                command: PathSpec::based(PathBase::Libexec, "nope"),
                args: vec![],
                env: BTreeMap::new(),
                stdin_path: None,
                stdout_path: None,
                chdir: None,
                allow_failure: false,
                sudo: Sudo::Never,
            },
        )
        .unwrap_err();
        assert!(missing.to_string().contains("libexec/nope"));
    }

    #[test]
    fn terminate_process_without_a_match_is_fine_unless_it_must_succeed() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "gnupg", "2");
        let step = |must_succeed| StepAction::TerminateProcess {
            name: "zerobrew-no-such-process-{{formula_name}}".into(),
            match_by: ProcessMatch::Name,
            attempts: 1,
            must_succeed,
            failure_message: Some("still running".into()),
            notices: vec!["stopping {{formula_name}}".into()],
            sudo: Sudo::Never,
        };
        run(&ctx, step(false)).unwrap();
        assert!(run(&ctx, step(true)).is_err());
    }

    #[test]
    fn install_gzipped_executable_unpacks_and_removes_the_source() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let gz = ctx.keg.join("libexec/tool.gz");
        fs::create_dir_all(gz.parent().unwrap()).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(b"#!/bin/sh\necho hi\n").unwrap();
        fs::write(&gz, encoder.finish().unwrap()).unwrap();
        let target = ctx.keg.join("bin/tool");
        run(
            &ctx,
            StepAction::InstallGzippedExecutable {
                source: abs(&gz),
                target: abs(&target),
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "#!/bin/sh\necho hi\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!gz.exists());
        // A missing source is skipped.
        run(
            &ctx,
            StepAction::InstallGzippedExecutable {
                source: abs(&gz),
                target: abs(&target),
            },
        )
        .unwrap();
    }

    #[test]
    fn formula_tools_must_exist() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let err = run(
            &ctx,
            StepAction::CompileGsettingsSchemas {
                path: abs(&ctx.prefix.join("share/glib-2.0/schemas")),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("glib is missing required executable")
        );

        let tool = ctx.prefix.join("opt/glib/bin/glib-compile-schemas");
        fs::create_dir_all(tool.parent().unwrap()).unwrap();
        fs::write(&tool, "#!/bin/sh\necho \"$1\" > \"$HOMEBREW_PREFIX/ran\"\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        run(
            &ctx,
            StepAction::CompileGsettingsSchemas {
                path: PathSpec::absolute("{{HOMEBREW_PREFIX}}/share/glib-2.0/schemas"),
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(ctx.prefix.join("ran")).unwrap().trim(),
            ctx.prefix.join("share/glib-2.0/schemas").to_str().unwrap()
        );
    }

    #[test]
    fn gcc_runtime_is_a_no_op_on_macos_and_unsupported_on_linux() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "gcc", "15.1.0");
        let result = run(&ctx, StepAction::ConfigureGccRuntime);
        if cfg!(target_os = "macos") {
            assert_eq!(result.unwrap(), None);
        } else {
            assert!(matches!(result, Err(StepError::Unsupported(_))));
        }
    }

    #[test]
    fn unsupported_steps_fail_with_their_reason() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let err = run(
            &ctx,
            StepAction::Unsupported {
                kind: "init_data_dir".into(),
                reason: "unknown step".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err, StepError::Unsupported("unknown step".into()));
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative_path(Path::new("/p/Cellar/x/1/bin/t"), Path::new("/p/bin")),
            PathBuf::from("../Cellar/x/1/bin/t")
        );
        assert_eq!(
            relative_path(Path::new("/p/a"), Path::new("/p")),
            PathBuf::from("a")
        );
        assert_eq!(
            relative_path(Path::new("/a/b"), Path::new("/c/d/e")),
            PathBuf::from("../../../a/b")
        );
    }
}
