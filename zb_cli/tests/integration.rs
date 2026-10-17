use std::path::PathBuf;
use std::process::{Command, Output};

struct TestEnv {
    root: tempfile::TempDir,
    /// On macOS, Mach-O binary patching requires the prefix path to be no longer
    /// than the original Homebrew prefix (`/opt/homebrew` = 13 chars). The default
    /// OS temp directory on macOS (`/var/folders/…`) produces paths far too long,
    /// so we create a separate short temp dir in `/tmp` for the prefix.
    prefix_dir: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            root: tempfile::TempDir::new().expect("failed to create temp dir"),
            prefix_dir: tempfile::Builder::new()
                .prefix("zb")
                .rand_bytes(3)
                .tempdir_in("/tmp")
                .expect("failed to create short prefix temp dir"),
        }
    }

    fn prefix(&self) -> PathBuf {
        self.prefix_dir.path().to_path_buf()
    }

    fn zb(&self, args: &[&str]) -> Output {
        let zb = env!("CARGO_BIN_EXE_zb");
        Command::new(zb)
            // Isolate shell configuration as well as package storage.
            .env("HOME", self.root.path())
            .env("SHELL", "/bin/sh")
            .env_remove("ZDOTDIR")
            .env("ZEROBREW_ROOT", self.root.path())
            // Use the short prefix so Mach-O patching stays within the 13-char limit,
            // and prevent a host-level ZEROBREW_PREFIX from leaking into the test.
            .env("ZEROBREW_PREFIX", self.prefix())
            .env("ZEROBREW_AUTO_INIT", "true")
            .args(args)
            .output()
            .unwrap_or_else(|_| panic!("failed to execute {zb} command"))
    }

    fn bin_dir(&self) -> PathBuf {
        self.prefix().join("bin")
    }

    fn count_store_entries(&self) -> usize {
        assert!(self.root.path().join("store").is_dir());
        std::fs::read_dir(self.root.path().join("store"))
            .map(|r| r.count())
            .expect("failed to read store directory")
    }

    fn run_binary(&self, name: &str, args: &[&str]) -> Output {
        let bin_path = self.bin_dir().join(name);
        Command::new(&bin_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin_dir().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to execute {}: {e}", bin_path.display()))
    }

    /// Find a binary inside the cellar (for keg-only formulas that aren't linked).
    fn cellar_binary(&self, formula: &str, binary: &str) -> PathBuf {
        let cellar = self.prefix().join("Cellar").join(formula);
        let versions: Vec<_> = std::fs::read_dir(&cellar)
            .unwrap_or_else(|e| panic!("no cellar entry for {formula}: {e}"))
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !versions.is_empty(),
            "no versions found in cellar for {formula}"
        );
        versions[0].path().join("bin").join(binary)
    }

    fn run_cellar_binary(&self, formula: &str, binary: &str, args: &[&str]) -> Output {
        let bin_path = self.cellar_binary(formula, binary);
        Command::new(&bin_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_path.parent().unwrap().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to execute {}: {e}", bin_path.display()))
    }
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{} failed:\nstdout: {}\nstderr: {}",
        context,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn auto_init_uses_isolated_shell_environment() {
    let t = TestEnv::new();
    // Force the real auto-init path; both TempDirs otherwise already exist.
    std::fs::remove_dir(t.prefix()).unwrap();
    let fish_config = t.root.path().join(".config/fish/conf.d/zerobrew.fish");
    std::fs::create_dir_all(fish_config.parent().unwrap()).unwrap();
    std::fs::write(&fish_config, "# keep this fish configuration\n").unwrap();
    assert_success(&t.zb(&["list"]), "zb list with isolated auto-init");
    assert!(t.root.path().join("store").is_dir());
    assert!(t.prefix().join("Cellar").is_dir());
    assert_eq!(
        std::fs::read_to_string(fish_config).unwrap(),
        "# keep this fish configuration\n"
    );
}

fn assert_stdout_contains(output: &Output, needle: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(needle),
        "expected stdout to contain {needle:?}, got: {stdout}"
    );
}

fn assert_no_installed_symlinks(dir: &std::path::Path) {
    if !dir.exists() {
        return;
    }
    let cellar = dir.join("Cellar");
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.expect("failed to read directory entry");
        if entry.path().starts_with(&cellar) {
            continue;
        }
        assert!(
            !entry.path_is_symlink(),
            "unexpected symlink: {}",
            entry.path().display()
        );
    }
}

#[test]
#[ignore = "integration test"]
#[cfg(target_os = "macos")] // GitHub Actions linux runner needs additional X11/XCB deps
fn test_ffmpeg_formula() {
    let t = TestEnv::new();

    assert_success(&t.zb(&["install", "ffmpeg"]), "zb install ffmpeg");

    // From the upstream formula test:
    // https://github.com/Homebrew/homebrew-core/blob/3076627c980d101ff02a720060c508433c44f293/Formula/f/ffmpeg.rb#L114
    let mp4out = t.root.path().join("video.mp4");
    assert_success(
        &t.run_binary(
            "ffmpeg",
            &[
                "-filter_complex",
                "testsrc=rate=1:duration=5",
                mp4out.to_str().unwrap(),
            ],
        ),
        "ffmpeg create test video",
    );
    assert!(mp4out.exists());
}

#[test]
#[ignore = "integration test"]
fn test_curl_keg_only() {
    let t = TestEnv::new();

    assert_success(&t.zb(&["install", "curl"]), "zb install curl");

    // curl is keg-only with reason :provided_by_macos.
    // On macOS the reason applies, so it should NOT be linked.
    // On Linux the reason is irrelevant, so it SHOULD be linked.
    if cfg!(target_os = "macos") {
        assert!(
            !t.bin_dir().join("curl").exists(),
            "curl should not be linked (keg-only on macOS)"
        );
    } else {
        assert!(
            t.bin_dir().join("curl").exists(),
            "curl should be linked on Linux (provided_by_macos is not applicable)"
        );
    }

    // the binary should exist in the cellar and work either way
    let output = if t.bin_dir().join("curl").exists() {
        t.run_binary("curl", &["https://www.githubstatus.com"])
    } else {
        t.run_cellar_binary("curl", "curl", &["https://www.githubstatus.com"])
    };
    assert_success(&output, "curl https://www.githubstatus.com");
    assert_stdout_contains(&output, "GitHub");
}

#[test]
#[ignore = "integration test"]
fn test_tap_source_build_with_class_level_build_options() {
    let t = TestEnv::new();

    // https://github.com/xo/homebrew-xo/blob/master/Formula/usql.rb
    // Source-only tap formula that calls `build.with?` in the class body.
    assert_success(&t.zb(&["install", "xo/xo/usql"]), "zb install xo/xo/usql");

    let output = t.run_binary("usql", &["--version"]);
    assert_success(&output, "usql --version");
    assert_stdout_contains(&output, "usql ");
}

#[test]
#[ignore = "integration test"]
fn test_install_uninstall_and_reinstall() {
    let t = TestEnv::new();

    assert_success(&t.zb(&["install", "jq"]), "zb install jq");

    let test_json = t.root.path().join("test.json");
    std::fs::write(&test_json, r#"{"foo":1, "bar":2}"#).expect("failed to write test.json");

    let output = t.run_binary("jq", &[".bar", test_json.to_str().unwrap()]);
    assert_success(&output, "jq .bar test.json");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "2\n");

    assert_success(&t.zb(&["uninstall", "jq"]), "zb uninstall jq");
    assert_success(&t.zb(&["uninstall", "oniguruma"]), "zb uninstall oniguruma");
    assert!(!t.bin_dir().join("jq").exists());
    assert_no_installed_symlinks(&t.prefix());

    assert_success(&t.zb(&["install", "jq"]), "zb install jq (reinstall)");
    assert_success(
        &t.run_binary("jq", &["--version"]),
        "jq --version after reinstall",
    );
}

#[test]
#[ignore = "integration test"]
fn test_list_installed_formulas() {
    let t = TestEnv::new();

    let output = t.zb(&["list"]);
    assert_success(&output, "zb list (empty)");
    assert_stdout_contains(&output, "No formulas installed");

    assert_success(&t.zb(&["install", "jq"]), "zb install jq");

    let output = t.zb(&["list"]);
    assert_success(&output, "zb list");
    assert_stdout_contains(&output, "jq");

    assert_success(&t.zb(&["uninstall", "jq"]), "zb uninstall jq");
    assert_success(&t.zb(&["uninstall", "oniguruma"]), "zb uninstall oniguruma");

    let output = t.zb(&["list"]);
    assert_success(&output, "zb list (empty)");
    assert_stdout_contains(&output, "No formulas installed");
}

#[test]
#[ignore = "integration test"]
fn test_info_finds_installed_formula() {
    let t = TestEnv::new();

    let output = t.zb(&["info", "jq"]);
    assert_success(&output, "zb info jq (not installed)");
    assert_stdout_contains(&output, "not installed");

    assert_success(&t.zb(&["install", "jq"]), "zb install jq");

    let output = t.zb(&["info", "jq"]);
    assert_success(&output, "zb info jq");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Name:") && !stdout.contains("not installed"),
        "stdout: {stdout}"
    );
}

#[test]
#[ignore = "integration test"]
fn test_gc_removes_unused_store_entries() {
    let t = TestEnv::new();

    assert_success(&t.zb(&["gc"]), "zb gc (empty)");
    assert_eq!(t.count_store_entries(), 0);

    assert_success(&t.zb(&["install", "jq"]), "zb install jq");
    let entries_before = t.count_store_entries();
    assert!(entries_before > 0);

    assert_success(&t.zb(&["uninstall", "jq"]), "zb uninstall jq");
    assert_success(&t.zb(&["uninstall", "oniguruma"]), "zb uninstall oniguruma");
    assert_eq!(t.count_store_entries(), entries_before);

    assert_success(&t.zb(&["gc"]), "zb gc");
    assert_eq!(t.count_store_entries(), 0);
}

#[test]
fn only_explicit_init_modifies_shell_configuration() {
    for (shell, config_name) in [
        ("/usr/bin/fish", ".config/fish/conf.d/zerobrew.fish"),
        ("/bin/bash", ".bashrc"),
        ("/bin/zsh", ".zshrc"),
    ] {
        for existing in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = temp.path().join("root");
            let prefix = temp.path().join("prefix");
            let config = home.join(config_name);
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            let original = "# keep this configuration\n# >>> zerobrew >>>\n# previous zerobrew settings\n# <<< zerobrew <<<\n";
            if existing {
                std::fs::write(&config, original).unwrap();
            }
            let before = std::fs::read(&config).ok();
            let run = |args: &[&str]| {
                Command::new(env!("CARGO_BIN_EXE_zb"))
                    .env("HOME", &home)
                    .env("SHELL", shell)
                    .env_remove("ZDOTDIR")
                    .env_remove("ZEROBREW_DIR")
                    .env_remove("ZEROBREW_AUTO_INIT")
                    .arg("--root")
                    .arg(&root)
                    .arg("--prefix")
                    .arg(&prefix)
                    .args(args)
                    .output()
                    .unwrap()
            };
            for args in [
                vec!["--auto-init", "list"],
                vec!["reset", "--yes"],
                vec!["init", "--no-modify-path"],
            ] {
                let output = run(&args);
                assert_success(&output, "initialization in isolated home");
                if args == ["init", "--no-modify-path"] {
                    assert!(
                        String::from_utf8_lossy(&output.stdout)
                            .contains("Skipped shell configuration (--no-modify-path)")
                    );
                }
                assert!(root.join("store").is_dir());
                assert!(prefix.join("Cellar").is_dir());
                assert_eq!(
                    std::fs::read(&config).ok(),
                    before,
                    "shell={shell}, existing={existing}, args={args:?}"
                );
            }
            assert_success(&run(&["init"]), "explicit zb init");
            let updated = std::fs::read_to_string(&config).unwrap();
            assert!(updated.contains(prefix.to_str().unwrap()));
            if existing {
                assert!(updated.starts_with("# keep this configuration\n"));
            }
        }
    }
}
