//! Derived from Homebrew's `Library/Homebrew/install_steps.rb` and
//! `install_steps/formula_actions.rb` (https://github.com/Homebrew/brew).
//! Copyright (c) 2009-present, Homebrew contributors. BSD 2-Clause License,
//! see LICENSE-HOMEBREW.
//!
//! `configure_php`: fix up pear/pecl permissions and paths, point pear's
//! system config at the prefix, and write `ext-opcache.ini` for `php@*`.

use super::steps::command_env;
use super::template::expand;
use super::{PostInstallContext, StepError};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) fn configure_php(ctx: &PostInstallContext) -> Result<(), StepError> {
    let pear_prefix = ctx.keg.join("share").join(&ctx.name).join("pear");
    let channels = [
        pear_prefix.join(".channels"),
        pear_prefix.join(".channels/.alias"),
    ];

    for dir in channels.iter().filter(|d| d.is_dir()) {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
            .map_err(failed("set permissions on", dir))?;
    }

    let mut pear_files: Vec<PathBuf> = [".depdblock", ".filemap", ".depdb", ".lock"]
        .iter()
        .map(|file| pear_prefix.join(file))
        .filter(|file| file.is_file())
        .collect();
    for dir in channels.iter().filter(|d| d.is_dir()) {
        for entry in fs::read_dir(dir).map_err(failed("read", dir))? {
            let entry = entry.map_err(failed("read", dir))?;
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                pear_files.push(entry.path());
            }
        }
    }
    for file in &pear_files {
        fs::set_permissions(file, fs::Permissions::from_mode(0o644))
            .map_err(failed("set permissions on", file))?;
    }

    let pecl_path = ctx.prefix.join("lib/php/pecl");
    fs::create_dir_all(&pecl_path).map_err(failed("create", &pecl_path))?;
    let keg_pecl = ctx.keg.join("pecl");
    if keg_pecl.is_symlink() {
        fs::remove_file(&keg_pecl).map_err(failed("remove", &keg_pecl))?;
    }
    if !keg_pecl.exists() {
        unix_fs::symlink(&pecl_path, &keg_pecl).map_err(failed("symlink", &keg_pecl))?;
    }

    let php_config = ctx.keg.join("bin/php-config");
    let extension_dir = run(ctx, &php_config, &["--extension-dir".into()])?;
    let php_basename = Path::new(extension_dir.trim())
        .file_name()
        .ok_or_else(|| {
            StepError::Failed(format!(
                "`{} --extension-dir` printed no directory",
                php_config.display()
            ))
        })?
        .to_owned();
    let pecl_ext_dir = pecl_path.join(&php_basename);
    fs::create_dir_all(&pecl_ext_dir).map_err(failed("create", &pecl_ext_dir))?;

    let version = expand(ctx, "{{version.major_minor}}")?;
    let opt_prefix = ctx.prefix.join("opt").join(&ctx.name);

    let pear_dir = if ctx.name == "php" {
        "pear".to_string()
    } else {
        format!("pear@{version}")
    };
    let pear_path = ctx.prefix.join("share").join(pear_dir);
    copy_dir_all(&pear_prefix, &pear_path).map_err(failed("copy", &pear_prefix))?;

    let php_ext_dir = opt_prefix.join("lib/php").join(&php_basename);
    let pear = ctx.keg.join("bin/pear");
    let settings: [(&str, PathBuf); 11] = [
        (
            "php_ini",
            ctx.prefix.join("etc/php").join(&version).join("php.ini"),
        ),
        ("php_dir", pear_path.clone()),
        ("doc_dir", pear_path.join("doc")),
        ("ext_dir", pecl_ext_dir.clone()),
        ("bin_dir", opt_prefix.join("bin")),
        ("data_dir", pear_path.join("data")),
        ("cfg_dir", pear_path.join("cfg")),
        ("www_dir", pear_path.join("htdocs")),
        ("man_dir", ctx.prefix.join("share/man")),
        ("test_dir", pear_path.join("test")),
        ("php_bin", opt_prefix.join("bin/php")),
    ];
    for (key, value) in &settings {
        if key.ends_with("_dir") && *key != "bin_dir" && *key != "man_dir" {
            fs::create_dir_all(value).map_err(failed("create", value))?;
        }
        run(
            ctx,
            &pear,
            &[
                "config-set".into(),
                (*key).into(),
                value.into(),
                "system".into(),
            ],
        )?;
    }
    run(ctx, &pear, &["update-channels".into()])?;

    if ctx.name == "php" {
        return Ok(());
    }

    let ext_config_path = ctx
        .prefix
        .join("etc/php")
        .join(&version)
        .join("conf.d/ext-opcache.ini");
    let conf_d = ext_config_path.parent().unwrap();
    fs::create_dir_all(conf_d).map_err(failed("create", conf_d))?;
    write_opcache_ini(&php_ext_dir, &ext_config_path).map_err(failed("write", &ext_config_path))
}

/// Run a php tool the way the other steps do: a private `HOME` so pear does
/// not write into the user's, and the prefix on `PATH`. Returns stdout.
fn run(ctx: &PostInstallContext, program: &Path, args: &[OsString]) -> Result<String, StepError> {
    if !program.is_file() {
        return Err(StepError::Failed(format!(
            "{} is missing required executable: {}",
            ctx.name,
            program.display()
        )));
    }
    let home = tempfile::Builder::new()
        .prefix(&format!("{}-postinstall-", ctx.name))
        .tempdir_in(&ctx.temp)
        .map_err(StepError::io("creating a temporary home"))?;
    let output = Command::new(program)
        .args(args)
        .envs(command_env(ctx, home.path()))
        .output()
        .map_err(StepError::io(format!("running {}", program.display())))?;
    if !output.status.success() {
        return Err(StepError::Failed(format!(
            "`{} {}` failed with {}\n{}",
            program.display(),
            args.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `zend_extension=` pointing at the keg's `opcache.so`, replacing any line
/// already there. OPcache is part of the binary as of PHP 8.5, so an absent
/// `opcache.so` leaves the file alone instead of pointing at nothing.
fn write_opcache_ini(php_ext_dir: &Path, ini: &Path) -> io::Result<()> {
    let so = php_ext_dir.join("opcache.so");
    if !so.is_file() {
        return Ok(());
    }
    let line = format!("zend_extension=\"{}\"", so.display());

    if ini.is_file() {
        let old = fs::read_to_string(ini)?;
        let trailing_nl = old.ends_with('\n');
        let mut new = old
            .lines()
            .map(|l| {
                if l.split_once('=')
                    .is_some_and(|(key, _)| key.trim() == "zend_extension")
                {
                    line.as_str()
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if trailing_nl {
            new.push('\n');
        }
        fs::write(ini, new)
    } else {
        fs::write(ini, format!("[opcache]\n{line}\n"))
    }
}

fn failed<'a>(action: &'a str, path: &'a Path) -> impl FnOnce(io::Error) -> StepError + 'a {
    move |err| {
        StepError::Failed(format!(
            "failed to {action} {} with error {err}",
            path.display()
        ))
    }
}

/// `cp -R "$src/." "$dst"`.
fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::post_install::test_support::context;
    use tempfile::TempDir;

    fn stub(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn lays_out_pear_and_pecl_like_homebrew() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "php@8.4", "8.4.14");
        let pear_prefix = ctx.keg.join("share/php@8.4/pear");
        fs::create_dir_all(pear_prefix.join(".channels/.alias")).unwrap();
        fs::write(pear_prefix.join(".channels/pear.xml"), "").unwrap();
        fs::write(pear_prefix.join(".lock"), "").unwrap();
        stub(
            &ctx.keg.join("bin/php-config"),
            "#!/bin/sh\necho /somewhere/lib/php/20240924\n",
        );
        let log = tmp.path().join("pear.log");
        stub(
            &ctx.keg.join("bin/pear"),
            &format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
        );
        // opt_prefix points at the keg, as `zb link` leaves it.
        let opt = ctx.prefix.join("opt");
        fs::create_dir_all(&opt).unwrap();
        unix_fs::symlink(&ctx.keg, opt.join("php@8.4")).unwrap();
        fs::create_dir_all(ctx.keg.join("lib/php/20240924")).unwrap();
        fs::write(ctx.keg.join("lib/php/20240924/opcache.so"), "").unwrap();

        configure_php(&ctx).unwrap();

        // The basename of --extension-dir, not the whole path.
        assert!(ctx.prefix.join("lib/php/pecl/20240924").is_dir());
        assert_eq!(
            fs::read_link(ctx.keg.join("pecl")).unwrap(),
            ctx.prefix.join("lib/php/pecl")
        );
        assert!(ctx.prefix.join("share/pear@8.4/doc").is_dir());
        assert!(ctx.prefix.join("share/pear@8.4/.lock").is_file());
        assert_eq!(mode(&pear_prefix.join(".channels")), 0o755);
        assert_eq!(mode(&pear_prefix.join(".lock")), 0o644);
        assert_eq!(mode(&pear_prefix.join(".channels/pear.xml")), 0o644);

        let log = fs::read_to_string(&log).unwrap();
        assert!(log.contains(&format!(
            "config-set php_ini {}/etc/php/8.4/php.ini system",
            ctx.prefix.display()
        )));
        assert!(log.contains(&format!(
            "config-set bin_dir {}/opt/php@8.4/bin system",
            ctx.prefix.display()
        )));
        assert!(log.contains("update-channels"));
        // bin_dir and man_dir are not created.
        assert!(!ctx.prefix.join("share/man").exists());

        assert_eq!(
            fs::read_to_string(ctx.prefix.join("etc/php/8.4/conf.d/ext-opcache.ini")).unwrap(),
            format!(
                "[opcache]\nzend_extension=\"{}/opt/php@8.4/lib/php/20240924/opcache.so\"\n",
                ctx.prefix.display()
            )
        );
    }

    #[test]
    fn unversioned_php_gets_no_opcache_ini() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "php", "8.4.14");
        fs::create_dir_all(ctx.keg.join("share/php/pear")).unwrap();
        stub(
            &ctx.keg.join("bin/php-config"),
            "#!/bin/sh\necho /x/20240924\n",
        );
        stub(&ctx.keg.join("bin/pear"), "#!/bin/sh\nexit 0\n");

        configure_php(&ctx).unwrap();

        assert!(ctx.prefix.join("share/pear").is_dir());
        assert!(!ctx.prefix.join("etc/php/8.4/conf.d").exists());
    }

    #[test]
    fn a_failing_pear_stops_the_step() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "php", "8.4.14");
        fs::create_dir_all(ctx.keg.join("share/php/pear")).unwrap();
        stub(
            &ctx.keg.join("bin/php-config"),
            "#!/bin/sh\necho /x/20240924\n",
        );
        stub(
            &ctx.keg.join("bin/pear"),
            "#!/bin/sh\necho nope >&2\nexit 1\n",
        );
        let err = configure_php(&ctx).unwrap_err();
        assert!(matches!(err, StepError::Failed(m) if m.contains("nope")));
    }
}
