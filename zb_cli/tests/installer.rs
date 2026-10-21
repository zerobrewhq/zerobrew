#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    output
}

fn isolated_command(home: &Path, shell: &str) -> Command {
    fs::create_dir_all(home.join("root")).unwrap();
    fs::create_dir_all(home.join("prefix")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_zb"));
    command
        .env_clear()
        .env("HOME", home)
        .env("SHELL", shell)
        .env("PATH", "/usr/bin:/bin")
        .arg("--root")
        .arg(home.join("root"))
        .arg("--prefix")
        .arg(home.join("prefix"));
    command
}

#[test]
fn init_preserves_binary_directory_in_shell_config_and_manual_instructions() {
    for (shell, config) in [
        ("/bin/bash", ".bashrc"),
        ("/bin/zsh", ".zshrc"),
        ("/usr/bin/fish", ".config/fish/conf.d/zerobrew.fish"),
    ] {
        for binary_dir in [None, Some(""), Some("custom bin/user's tools")] {
            let home = tempfile::tempdir().unwrap();
            let destination = binary_dir
                .filter(|value| !value.is_empty())
                .map(|value| home.path().join(value));
            let expected = destination
                .clone()
                .unwrap_or_else(|| home.path().join(".zerobrew/bin"));
            let mut command = isolated_command(home.path(), shell);
            if let Some(value) = binary_dir {
                command.env(
                    "ZEROBREW_BIN",
                    destination.as_deref().unwrap_or(Path::new(value)),
                );
            }
            run(command.arg("init"));
            let before = fs::read(home.path().join(config)).unwrap();
            let output = run(command.arg("--no-modify-path"));
            assert_eq!(fs::read(home.path().join(config)).unwrap(), before);
            // Source the POSIX config to verify quoting as well as the value.
            if shell != "/usr/bin/fish" {
                let output = run(Command::new("/bin/sh")
                    .env_clear()
                    .env("HOME", home.path())
                    .arg("-c")
                    .arg(". \"$1\"; printf '%s' \"$ZEROBREW_BIN\"")
                    .arg("sh")
                    .arg(home.path().join(config)));
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    expected.to_str().unwrap()
                );
            } else {
                let config = String::from_utf8(before).unwrap();
                assert!(
                    config.contains(&format!("set -gx ZEROBREW_BIN \"{}\"", expected.display()))
                );
            }
            let instructions = String::from_utf8(output.stdout).unwrap();
            let bin_assignment = if shell == "/usr/bin/fish" {
                "set -gx ZEROBREW_BIN "
            } else {
                "export ZEROBREW_BIN="
            };
            assert!(instructions.contains(bin_assignment), "{instructions}");
        }
    }
}

#[test]
fn installer_preserves_binary_directory_when_rerun() {
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../install.sh");
    for binary_dir in [
        None,
        Some(""),
        Some(".zerobrew/bin"),
        Some("custom bin/user's tools"),
    ] {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("root")).unwrap();
        fs::create_dir(home.path().join("prefix")).unwrap();
        let sources = home.path().join("sources");
        fs::create_dir(&sources).unwrap();
        // The real installer and init code run unchanged. Only redirect the
        // CLI's data paths, since the macOS installer selects /opt/zerobrew.
        let source = sources.join("zb");
        fs::write(
            &source,
            "#!/bin/sh\nexec \"$TEST_ZB\" --root \"$HOME/root\" --prefix \"$HOME/prefix\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(home.path().join(".bashrc"), "# keep user configuration\n").unwrap();
        let expected = home.path().join(
            binary_dir
                .filter(|value| !value.is_empty())
                .unwrap_or(".local/bin"),
        );
        let mut command = Command::new("/bin/bash");
        command
            .env_clear()
            .env("HOME", home.path())
            .env("SHELL", "/bin/bash")
            .env("PATH", "/usr/bin:/bin")
            .env("TEST_ZB", env!("CARGO_BIN_EXE_zb"))
            .env("TEST_INSTALLER", &installer)
            .env("TEST_SOURCE", &source)
            .env("TEST_EXPECT_BIN", &expected)
            .arg("-c")
            .arg(
                "set -eu
for attempt in 1 2; do
    /bin/bash \"$TEST_INSTALLER\" --binary \"$TEST_SOURCE\"
    . \"$HOME/.bashrc\"
    test \"$ZEROBREW_BIN\" = \"$TEST_EXPECT_BIN\"
    test \"$(command -v zb)\" = \"$TEST_EXPECT_BIN/zb\"
    zb --version
done",
            );
        if let Some(value) = binary_dir {
            command.env(
                "ZEROBREW_BIN",
                if value.is_empty() {
                    Path::new("")
                } else {
                    &expected
                },
            );
        }
        run(&mut command);
        assert!(expected.join("zb").is_file());
        for alternate in [".local/bin", ".zerobrew/bin"] {
            let alternate = home.path().join(alternate);
            if alternate != expected {
                assert!(!alternate.join("zb").exists());
            }
        }
        let config = fs::read_to_string(home.path().join(".bashrc")).unwrap();
        assert!(config.starts_with("# keep user configuration\n"));
        assert_eq!(config.matches("# >>> zerobrew >>>").count(), 1);
    }
}
