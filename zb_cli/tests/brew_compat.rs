#![cfg(unix)]

use std::fs;
use std::os::unix::{
    fs::{PermissionsExt, symlink},
    process::ExitStatusExt,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{PoisonError, RwLock};

/// Locate bash without assuming the FHS layout. Termux (Android) has no
/// `/bin/bash`, only `$TERMUX__PREFIX/bin/bash`.
fn bash() -> PathBuf {
    let candidate = Path::new("/bin/bash");
    if candidate.exists() {
        return candidate.to_path_buf();
    }
    if let Ok(prefix) = std::env::var("TERMUX__PREFIX") {
        let candidate = Path::new(&prefix).join("bin/bash");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("bash")
}

/// A child forked by another test thread keeps any open write descriptor until
/// it execs, and executing that file in the meantime fails with ETXTBSY. Writes
/// to executables take this lock exclusively and spawns share it.
static EXEC: RwLock<()> = RwLock::new(());

fn write_executables<T>(write: impl FnOnce() -> T) -> T {
    let _guard = EXEC.write().unwrap_or_else(PoisonError::into_inner);
    write()
}

fn run(command: &mut Command) -> Output {
    let _guard = EXEC.read().unwrap_or_else(PoisonError::into_inner);
    command.output().unwrap()
}

fn entrypoint() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_executables(|| {
        fs::copy(env!("CARGO_BIN_EXE_zb-brew"), dir.path().join("zb-brew")).unwrap()
    });
    symlink("zb-brew", dir.path().join("brew")).unwrap();
    dir
}

#[test]
fn probe_signal_status() {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if line.starts_with("SigBlk") || line.starts_with("SigIgn") || line.starts_with("SigCgt") {
            eprintln!("PROBE {line}");
        }
    }
}

#[test]
fn probe_sh_signal() {
    let out = Command::new("/bin/sh")
        .args(["-c", "kill -TERM $$"])
        .output()
        .unwrap();
    eprintln!(
        "PROBE_SH status={:?} code={:?} signal={:?}",
        out.status,
        out.status.code(),
        out.status.signal()
    );
    let mut vars: Vec<_> = std::env::vars().collect();
    vars.sort();
    for (k, v) in vars {
        if [
            "PATH",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "SHELL",
            "PPID",
            "TMPDIR",
            "RUST_TEST_THREADS",
        ]
        .contains(&k.as_str())
        {
            eprintln!("PROBE_ENV {k}={v}");
        }
    }
}

#[test]
fn forwards_to_adjacent_zb_and_preserves_arguments_status_and_signal() {
    let dir = entrypoint();
    let zb = dir.path().join("zb");
    write_executables(|| {
        fs::write(&zb, "#!/bin/sh\nenv | sort > \"$ZBTEST_ENV_DUMP\"\nfor arg in \"$@\"; do echo \"$arg\"; done\nif [ \"${TEST_ZB_SIGNAL:-}\" = yes ]; then kill -TERM $$; fi\nexit \"${TEST_ZB_EXIT:-0}\"\n").unwrap()
    });
    fs::set_permissions(&zb, fs::Permissions::from_mode(0o755)).unwrap();
    let brew = dir.path().join("brew");
    let output = run(Command::new(&brew)
        .env("PATH", "")
        .env("TEST_ZB_EXIT", "17")
        .env(
            "ZBTEST_ENV_DUMP",
            std::env::temp_dir().join("zb_env_dump.txt"),
        )
        .args(["install", "a b", "c", "--no-link"]));
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(output.stdout, b"install\na b\nc\n--no-link\n");
    let output = run(Command::new(&brew).args(["cleanup"]));
    assert!(output.status.success());
    assert_eq!(output.stdout, b"gc\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("only unreferenced store entries"));
    let output = run(Command::new(&brew)
        .env("TEST_ZB_SIGNAL", "yes")
        .args(["list"]));
    eprintln!(
        "DEBUG signal status={:?} code={:?} signal={:?} stdout={:?} stderr={:?}",
        output.status,
        output.status.code(),
        output.status.signal(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.signal(), Some(15));
}

#[test]
fn unsupported_commands_and_flags_fail_before_executing_zb() {
    let dir = entrypoint();
    for args in [
        vec!["search", "jq"],
        vec!["services", "list"],
        vec!["install", "--cask", "foo"],
        vec!["list", "--formula"],
        vec!["list", "--cask"],
        vec!["cleanup", "--prune=all"],
    ] {
        let output = run(Command::new(dir.path().join("brew")).args(args));
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("cannot execute adjacent zb"));
    }
    for args in [vec!["--help"], vec!["install", "--help"], vec!["--version"]] {
        let output = run(Command::new(dir.path().join("brew")).args(args));
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("zerobrew"));
    }
    let output = run(Command::new(dir.path().join("brew")).args(["list"]));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("reinstall zerobrew"));
}

#[test]
fn removal_does_not_initialize_zerobrew_or_need_the_companion() {
    let dir = tempfile::tempdir().unwrap();
    write_executables(|| fs::copy(env!("CARGO_BIN_EXE_zb"), dir.path().join("zb")).unwrap());
    let root = dir.path().join("unused-root");
    symlink("zb-brew", dir.path().join("brew")).unwrap();
    let output = run(Command::new(dir.path().join("zb"))
        .env("ZEROBREW_ROOT", &root)
        .args(["install-alias", "brew", "--remove"]));
    assert!(output.status.success(), "{:?}", output);
    assert!(!root.exists());
    assert!(fs::symlink_metadata(dir.path().join("brew")).is_err());
    let output = run(Command::new(dir.path().join("zb")).args(["install-alias", "other"]));
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn migrate_refuses_the_compatibility_entrypoint_before_installing() {
    let dir = entrypoint();
    let root = dir.path().join("root");
    let prefix = dir.path().join("prefix");
    let output = run(Command::new(env!("CARGO_BIN_EXE_zb"))
        .env("HOME", dir.path())
        .env("SHELL", "/bin/sh")
        .env_remove("ZDOTDIR")
        .env("PATH", dir.path())
        .env("ZEROBREW_ROOT", &root)
        .env("ZEROBREW_PREFIX", &prefix)
        .args(["--auto-init", "migrate", "--yes"]));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not real Homebrew"),
        "{:?}",
        output
    );
    assert_eq!(fs::read_dir(prefix.join("Cellar")).unwrap().count(), 0);
}

#[test]
fn installer_keeps_brew_opt_in_for_explicit_and_release_binaries() {
    let installer = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../install.sh");
    for release in [false, true] {
        for enable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let sources = dir.path().join("sources");
            let tools = dir.path().join("tools");
            let destination = dir.path().join("bin");
            fs::create_dir(&sources).unwrap();
            fs::create_dir(&tools).unwrap();
            let log = dir.path().join("calls");
            let zb = sources.join("zb");
            let curl = tools.join("curl");
            write_executables(|| {
                fs::write(&zb, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo zerobrew-test; exit 0; fi\necho \"$*\" >> \"$TEST_INSTALL_LOG\"\n").unwrap();
                fs::set_permissions(&zb, fs::Permissions::from_mode(0o755)).unwrap();
                for name in ["zbx", "zb-brew"] {
                    fs::copy(&zb, sources.join(name)).unwrap();
                }
                fs::write(&curl, "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = -o ]; then destination=$2; shift; fi\n  shift\ndone\n/bin/cp \"$TEST_INSTALL_SOURCES/${destination##*/}\" \"$destination\"\n").unwrap();
                fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
            });
            let mut command = Command::new(bash());
            command
                .arg(&installer)
                .arg("--no-modify-path")
                .env("HOME", dir.path())
                .env("ZEROBREW_BIN", &destination)
                .env("ZEROBREW_DIR", dir.path().join("checkout"))
                .env("TEST_INSTALL_LOG", &log)
                .env("TEST_INSTALL_SOURCES", &sources)
                .env("PATH", format!("{}:/usr/bin:/bin", tools.display()));
            if enable {
                command.arg("--brew-compat");
            }
            if !release {
                command
                    .arg("--binary")
                    .arg(&zb)
                    .arg(sources.join("zbx"))
                    .arg(sources.join("zb-brew"));
            }
            let output = run(&mut command);
            assert!(
                output.status.success(),
                "release={release}, enable={enable}: {:?}",
                output
            );
            assert!(destination.join("zb-brew").exists());
            let calls = fs::read_to_string(&log).unwrap();
            assert!(calls.contains("init --no-modify-path"));
            assert_eq!(calls.contains("install-alias brew"), enable);
            assert!(!destination.join("brew").exists());
        }
    }
}
