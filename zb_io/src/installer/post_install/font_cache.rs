//! The font cache fontconfig builds in post-install, kept between installs.
//!
//! The formula's step runs `fc-cache --force --really-force`, which rescans
//! every font directory fontconfig is configured with and writes a cache
//! file per directory under `var/cache/fontconfig`. That takes a couple of
//! seconds on macOS, and fontconfig is a dependency of many formulae, so
//! every warm install of one of them paid it again.
//!
//! The cache depends only on fontconfig itself, its configuration and the
//! font directories it names. The step runs with a fresh `HOME`, so the
//! directories under `~` and the XDG ones are empty and play no part. The
//! rest are keyed on the path and modification time of every directory
//! below them, which is also what fontconfig checks to decide whether a
//! cache file is still valid. On a hit the cache files are copied back
//! instead of running `fc-cache`; on a miss it runs unchanged and its
//! output is kept.
//!
//! A configuration this module cannot read with certainty, such as a
//! relative `<dir>` or an environment that points fontconfig elsewhere,
//! leaves the step to run every time.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::{PostInstallContext, StepError};

/// Variables that make fontconfig read a configuration or font directory
/// other than the one under the prefix.
const FONTCONFIG_ENV: &[&str] = &[
    "FONTCONFIG_FILE",
    "FONTCONFIG_PATH",
    "FONTCONFIG_SYSROOT",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
];

static DIR_ELEMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<dir(\s[^>]*)?>([^<]*)</dir>").unwrap());
static INCLUDE_ELEMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<include(\s[^>]*)?>([^<]*)</include>").unwrap());
static PREFIX_ATTRIBUTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"prefix\s*=\s*["']([^"']*)["']"#).unwrap());

pub(super) struct FontCache {
    /// `var/cache/fontconfig`, the first `<cachedir>` of the configuration
    /// the formula installs.
    destination: PathBuf,
    cached: PathBuf,
}

impl FontCache {
    /// Recognise fontconfig's `fc-cache` step and work out its cache key.
    /// `None` for any other command, and for a step or configuration whose
    /// result the key cannot account for.
    pub(super) fn detect(
        ctx: &PostInstallContext,
        program: &OsStr,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Option<Self> {
        if ctx.name != "fontconfig" || !env.is_empty() {
            return None;
        }
        if Path::new(program).file_name() != Some(OsStr::new("fc-cache")) {
            return None;
        }
        if FONTCONFIG_ENV
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return None;
        }

        let mut hasher = Sha256::new();
        hasher.update(ctx.prefix.as_os_str().as_encoded_bytes());
        hasher.update(ctx.keg.as_os_str().as_encoded_bytes());
        for arg in args {
            hasher.update(arg.as_bytes());
            hasher.update([0]);
        }

        let config_root = ctx.prefix.join("etc/fonts");
        let mut font_dirs = Vec::new();
        for file in config_files(&config_root) {
            let content = fs::read_to_string(&file).ok()?;
            hasher.update(file.as_os_str().as_encoded_bytes());
            hasher.update(content.as_bytes());
            font_dirs.extend(scanned_dirs(&content, &config_root)?);
        }
        if font_dirs.is_empty() {
            return None;
        }
        font_dirs.sort();
        font_dirs.dedup();

        let mut seen = HashSet::new();
        for dir in &font_dirs {
            hash_tree(&mut hasher, dir, &mut seen);
        }

        let key: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Some(Self {
            destination: ctx.prefix.join("var/cache/fontconfig"),
            cached: ctx.cache.join("fontconfig").join(key),
        })
    }

    /// Put the cached files in place. `false` when there are none.
    pub(super) fn restore(&self) -> Result<bool, StepError> {
        if !self.cached.is_dir() {
            return Ok(false);
        }
        copy_files(&self.cached, &self.destination)?;
        Ok(true)
    }

    /// Remember the cache files `fc-cache` just wrote.
    pub(super) fn store(&self) -> Result<(), StepError> {
        if !self.destination.is_dir() || self.cached.is_dir() {
            return Ok(());
        }
        let parent = self.cached.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(StepError::io("creating cache directory"))?;
        // Fill a scratch directory and rename it, so an interrupted store
        // never leaves a partial cache behind under the key.
        let staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(parent)
            .map_err(StepError::io("creating cache directory"))?;
        copy_files(&self.destination, staging.path())?;
        match fs::rename(staging.path(), &self.cached) {
            Ok(()) => {
                // The directory is in place now; keep `TempDir` from removing it.
                let _ = staging.keep();
                Ok(())
            }
            // Another install stored the same key first.
            Err(_) if self.cached.is_dir() => Ok(()),
            Err(err) => Err(StepError::Failed(format!(
                "storing the font cache in {}: {err}",
                self.cached.display()
            ))),
        }
    }
}

/// `fonts.conf` and everything else under `etc/fonts`, in a stable order.
/// That covers `conf.d` and `local.conf`, the includes the formula's
/// configuration has.
fn config_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => pending.push(path),
                Ok(metadata) if metadata.is_file() => files.push(path),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

/// The font directories one configuration file adds that exist outside the
/// step's temporary `HOME`. `None` when the file names a directory or an
/// include this module cannot resolve.
fn scanned_dirs(content: &str, config_root: &Path) -> Option<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for element in DIR_ELEMENT.captures_iter(content) {
        let prefix = element
            .get(1)
            .and_then(|attributes| PREFIX_ATTRIBUTE.captures(attributes.as_str()))
            .map(|prefix| prefix[1].to_string());
        let path = element[2].trim();
        match prefix.as_deref() {
            // Under the temporary `HOME`, which is empty.
            Some("xdg") => continue,
            None | Some("default") if path.starts_with('~') => continue,
            None | Some("default") if path.starts_with('/') => dirs.push(PathBuf::from(path)),
            _ => return None,
        }
    }
    for element in INCLUDE_ELEMENT.captures_iter(content) {
        let prefix = element
            .get(1)
            .and_then(|attributes| PREFIX_ATTRIBUTE.captures(attributes.as_str()))
            .map(|prefix| prefix[1].to_string());
        let path = element[2].trim();
        match prefix.as_deref() {
            Some("xdg") => continue,
            None | Some("default") if path.starts_with('~') => continue,
            // Relative includes resolve against `etc/fonts`, whose files
            // are all read already.
            None | Some("default") if !path.starts_with('/') => continue,
            None | Some("default") if Path::new(path).starts_with(config_root) => continue,
            _ => return None,
        }
    }
    Some(dirs)
}

/// Hash the path and modification time of `dir` and every directory below
/// it, following symlinks as fontconfig does. A missing directory hashes
/// as such, so creating it later is a new key.
fn hash_tree(hasher: &mut Sha256, dir: &Path, seen: &mut HashSet<(u64, u64)>) {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        hasher.update(dir.as_os_str().as_encoded_bytes());
        let Ok(metadata) = fs::metadata(&dir) else {
            hasher.update(b"missing");
            continue;
        };
        if !metadata.is_dir() || !seen.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|age| age.as_nanos())
            .unwrap_or(0);
        hasher.update(modified.to_le_bytes());

        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        // Popped from the end, so reverse to walk in sorted order.
        children.sort_unstable_by(|a, b| b.cmp(a));
        pending.extend(children);
    }
}

/// Copy the regular files directly in `source` into `destination`.
fn copy_files(source: &Path, destination: &Path) -> Result<(), StepError> {
    fs::create_dir_all(destination)
        .map_err(StepError::io(format!("creating {}", destination.display())))?;
    let entries =
        fs::read_dir(source).map_err(StepError::io(format!("reading {}", source.display())))?;
    for entry in entries {
        let entry = entry.map_err(StepError::io(format!("reading {}", source.display())))?;
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let target = destination.join(entry.file_name());
        fs::copy(entry.path(), &target)
            .map_err(StepError::io(format!("copying to {}", target.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::steps::run_step;
    use super::super::test_support::context;
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;
    use zb_core::{PathBase, PathSpec, StepAction, Sudo};

    /// A stand-in for `fc-cache`: counts its runs and writes one cache file
    /// per font file it finds, named after it.
    fn install_fc_cache(ctx: &PostInstallContext) {
        let script = ctx.keg.join("bin/fc-cache");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(
            &script,
            "#!/bin/sh\n\
             echo run >> \"$HOMEBREW_PREFIX/runs\"\n\
             out=\"$HOMEBREW_PREFIX/var/cache/fontconfig\"\n\
             mkdir -p \"$out\"\n\
             for font in \"$HOMEBREW_PREFIX\"/fonts/*; do\n\
               [ -e \"$font\" ] && echo cached > \"$out/$(basename \"$font\").cache\"\n\
             done\n\
             echo tag > \"$out/CACHEDIR.TAG\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The formula's configuration, scanning `<prefix>/fonts` and the user
    /// directories the temporary `HOME` leaves empty.
    fn install_config(ctx: &PostInstallContext, extra: &str) -> PathBuf {
        let fonts = ctx.prefix.join("fonts");
        fs::create_dir_all(fonts.join("sub")).unwrap();
        let conf_d = ctx.prefix.join("etc/fonts/conf.d");
        fs::create_dir_all(&conf_d).unwrap();
        fs::write(
            ctx.prefix.join("etc/fonts/fonts.conf"),
            format!(
                "<fontconfig>\n\
                 \t<dir>{}</dir>\n\
                 \t<dir>~/Library/Fonts</dir>\n\
                 \t<dir prefix=\"xdg\">fonts</dir>\n\
                 \t{extra}\n\
                 \t<include ignore_missing=\"yes\">conf.d</include>\n\
                 \t<cachedir>{}/var/cache/fontconfig</cachedir>\n\
                 </fontconfig>\n",
                fonts.display(),
                ctx.prefix.display()
            ),
        )
        .unwrap();
        fs::write(
            conf_d.join("50-user.conf"),
            "<fontconfig><include ignore_missing=\"yes\" prefix=\"xdg\">fontconfig/conf.d</include></fontconfig>\n",
        )
        .unwrap();
        fonts
    }

    fn fc_cache_step() -> StepAction {
        StepAction::Run {
            command: PathSpec::based(PathBase::Bin, "fc-cache"),
            args: vec![
                "--force".into(),
                "--really-force".into(),
                "--verbose".into(),
            ],
            env: BTreeMap::new(),
            stdin_path: None,
            stdout_path: None,
            chdir: None,
            allow_failure: false,
            sudo: Sudo::Never,
        }
    }

    fn runs(ctx: &PostInstallContext) -> usize {
        fs::read_to_string(ctx.prefix.join("runs"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn touch_dir(dir: &Path, offset: u64) {
        File::open(dir)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + offset))
            .unwrap();
    }

    fn detect(ctx: &PostInstallContext) -> Option<FontCache> {
        let args = ["--force".to_string(), "--really-force".to_string()];
        FontCache::detect(ctx, OsStr::new("/x/bin/fc-cache"), &args, &BTreeMap::new())
    }

    #[test]
    fn fc_cache_runs_once_and_is_restored_after() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "fontconfig", "2.18.3");
        install_fc_cache(&ctx);
        let fonts = install_config(&ctx, "");
        fs::write(fonts.join("a.ttf"), "font").unwrap();
        let out = ctx.prefix.join("var/cache/fontconfig");

        run_step(&ctx, &fc_cache_step()).unwrap();
        assert_eq!(runs(&ctx), 1);
        assert_eq!(
            fs::read_to_string(out.join("a.ttf.cache")).unwrap(),
            "cached\n"
        );

        fs::remove_dir_all(&out).unwrap();
        run_step(&ctx, &fc_cache_step()).unwrap();
        assert_eq!(runs(&ctx), 1, "the second run comes from the cache");
        assert_eq!(
            fs::read_to_string(out.join("a.ttf.cache")).unwrap(),
            "cached\n"
        );
        assert!(out.join("CACHEDIR.TAG").is_file());
    }

    #[test]
    fn a_change_in_a_font_directory_runs_fc_cache_again() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "fontconfig", "2.18.3");
        install_fc_cache(&ctx);
        let fonts = install_config(&ctx, "");
        fs::write(fonts.join("a.ttf"), "font").unwrap();
        touch_dir(&fonts, 0);

        run_step(&ctx, &fc_cache_step()).unwrap();
        fs::write(fonts.join("b.ttf"), "font").unwrap();
        touch_dir(&fonts, 1);
        run_step(&ctx, &fc_cache_step()).unwrap();
        assert_eq!(runs(&ctx), 2);
        assert!(
            ctx.prefix
                .join("var/cache/fontconfig/b.ttf.cache")
                .is_file()
        );
    }

    #[test]
    fn the_key_covers_subdirectories_and_configuration() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "fontconfig", "2.18.3");
        let fonts = install_config(&ctx, "");
        touch_dir(&fonts.join("sub"), 0);
        let first = detect(&ctx).unwrap().cached;
        assert_eq!(detect(&ctx).unwrap().cached, first, "the key is stable");

        touch_dir(&fonts.join("sub"), 1);
        let second = detect(&ctx).unwrap().cached;
        assert_ne!(first, second, "a nested directory is part of the key");

        fs::write(
            ctx.prefix.join("etc/fonts/conf.d/99-local.conf"),
            "<fontconfig/>",
        )
        .unwrap();
        assert_ne!(
            detect(&ctx).unwrap().cached,
            second,
            "so is the configuration"
        );
    }

    #[test]
    fn configurations_it_cannot_read_are_left_to_fc_cache() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "fontconfig", "2.18.3");
        install_config(&ctx, "<dir prefix=\"relative\">fonts</dir>");
        assert!(detect(&ctx).is_none());

        install_config(&ctx, "<dir>fonts</dir>");
        assert!(detect(&ctx).is_none());

        install_config(&ctx, "<include>/etc/fonts/local.conf</include>");
        assert!(detect(&ctx).is_none());

        install_config(&ctx, "");
        assert!(detect(&ctx).is_some());
    }

    #[test]
    fn detect_ignores_other_commands_and_formulae() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "fontconfig", "2.18.3");
        install_config(&ctx, "");
        let no_env = BTreeMap::new();
        assert!(FontCache::detect(&ctx, OsStr::new("/bin/sh"), &[], &no_env).is_none());
        let env = BTreeMap::from([("FONTCONFIG_FILE".to_string(), "/x".to_string())]);
        assert!(FontCache::detect(&ctx, OsStr::new("fc-cache"), &[], &env).is_none());

        let other = context(tmp.path(), "harfbuzz", "1");
        assert!(FontCache::detect(&other, OsStr::new("fc-cache"), &[], &no_env).is_none());

        let empty = context(&tmp.path().join("empty"), "fontconfig", "2.18.3");
        assert!(
            FontCache::detect(&empty, OsStr::new("fc-cache"), &[], &no_env).is_none(),
            "without a configuration there is nothing to key on"
        );
    }

    #[test]
    fn a_failed_run_stores_nothing() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "fontconfig", "2.18.3");
        install_config(&ctx, "");
        let script = ctx.keg.join("bin/fc-cache");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(run_step(&ctx, &fc_cache_step()).is_err());
        assert!(!ctx.cache.join("fontconfig").exists());
    }
}
