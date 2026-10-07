//! Parity tests: install the same formula with Homebrew and with zerobrew, then
//! compare what landed on disk. Homebrew is the reference implementation, so
//! anything zerobrew writes differently is a bug unless it is on the allowlist.
//!
//! Needs a `brew` on `PATH`, or one named by `ZB_PARITY_BREW`. Without one the
//! tests skip, unless `ZB_PARITY_REQUIRE_BREW` is set, in which case they fail.
//! CI sets that so a missing Homebrew can't silently turn the suite off.
//!
//! The tests share the runner's Homebrew, which locks per command, so they are
//! behind the `parity` feature and run one at a time:
//!
//!     cargo test -p zb_cli --features parity --test parity -- --test-threads=1
//!
//! A plain `cargo test --workspace` leaves them out. `install_timings` compares
//! install times instead of files; CI runs it in a separate job on a fresh
//! runner with a release build, see `.github/workflows/parity.yml`.
//!
//! macOS only for now: binaries are compared through `otool` and `codesign`.
#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Files Homebrew writes into a keg that zerobrew doesn't, or writes differently.
/// Each entry says why.
const KEG_ALLOWLIST: &[(&str, &str)] = &[
    (
        "INSTALL_RECEIPT.json",
        "zerobrew records installs in its database instead of a receipt",
    ),
    (
        "sbom.spdx.json",
        "Homebrew regenerates the bottle's SBOM at install time with a timestamp",
    ),
];

struct Brew {
    bin: PathBuf,
    prefix: PathBuf,
}

impl Brew {
    /// The reference Homebrew, or `None` when there isn't one and the test may skip.
    fn find() -> Option<Self> {
        let bin = std::env::var_os("ZB_PARITY_BREW")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("PATH").and_then(|path| {
                    std::env::split_paths(&path)
                        .map(|dir| dir.join("brew"))
                        .find(|candidate| candidate.is_file())
                })
            });
        let Some(bin) = bin else {
            if std::env::var_os("ZB_PARITY_REQUIRE_BREW").is_some() {
                panic!(
                    "ZB_PARITY_REQUIRE_BREW is set but no `brew` was found on PATH or in ZB_PARITY_BREW"
                );
            }
            eprintln!("skipping: no `brew` on PATH and ZB_PARITY_BREW is unset");
            return None;
        };
        let prefix = PathBuf::from(stdout(&Self::command(&bin, &["--prefix"]), "brew --prefix"));
        Some(Self { bin, prefix })
    }

    fn command(bin: &Path, args: &[&str]) -> Output {
        Command::new(bin)
            .args(args)
            .env("HOMEBREW_NO_ANALYTICS", "1")
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
            .env("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK", "1")
            .output()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", bin.display()))
    }

    fn run(&self, args: &[&str]) -> Output {
        Self::command(&self.bin, args)
    }

    /// Install `formula` and return its keg path.
    /// Install `formula` at the version the API currently serves and return
    /// its keg path.
    ///
    /// CI runners ship a Homebrew with a stale API cache and some formulae
    /// preinstalled, so a plain `brew install` can be a no-op at an older
    /// version than the one zb resolves. Refresh the API first and upgrade
    /// whatever is already there. A reference Homebrew named by
    /// `ZB_PARITY_BREW` is treated as pinned and is not updated.
    fn install(&self, formula: &str) -> PathBuf {
        if std::env::var_os("ZB_PARITY_BREW").is_none() {
            assert_success(&self.run(&["update", "--quiet"]), "brew update");
        }
        assert_success(
            &self.run(&["install", "--formula", formula]),
            &format!("brew install {formula}"),
        );
        // No-op when the install above was fresh or already current.
        assert_success(
            &self.run(&["upgrade", "--formula", formula]),
            &format!("brew upgrade {formula}"),
        );
        // `opt/<formula>` points at the current keg even when older
        // versions are still in the Cellar.
        let opt = PathBuf::from(stdout(
            &self.run(&["--prefix", formula]),
            &format!("brew --prefix {formula}"),
        ));
        fs::canonicalize(&opt).unwrap_or_else(|e| panic!("cannot resolve {}: {e}", opt.display()))
    }
}

struct Zb {
    /// The `zb` under test, or a released one when timing a baseline.
    bin: PathBuf,
    root: tempfile::TempDir,
    /// Short on purpose: Mach-O patching needs a prefix no longer than
    /// `/opt/homebrew`, and the default temp dir on macOS is far longer.
    prefix_dir: tempfile::TempDir,
}

impl Zb {
    fn new() -> Self {
        Self::with_binary(PathBuf::from(env!("CARGO_BIN_EXE_zb")))
    }

    fn with_binary(bin: PathBuf) -> Self {
        Self {
            bin,
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.bin)
            .env("ZEROBREW_ROOT", self.root.path())
            .env("ZEROBREW_PREFIX", self.prefix())
            .env("ZEROBREW_AUTO_INIT", "true")
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", self.bin.display()))
    }

    fn install(&self, formula: &str) -> PathBuf {
        assert_success(
            &self.run(&["install", formula]),
            &format!("zb install {formula}"),
        );
        single_version_dir(&self.prefix().join("Cellar").join(formula))
    }
}

/// Everything about a file that should match between the two installs.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    Symlink(String),
    File {
        mode: u32,
        content: Vec<u8>,
    },
    /// Mach-O binaries are compared by their load commands, not their bytes:
    /// install names embed the prefix and the ad-hoc signature covers them.
    MachO {
        mode: u32,
        load_commands: String,
    },
}

/// Snapshot a directory tree, with the install prefix replaced by a marker so
/// the two trees are comparable.
fn snapshot(root: &Path, prefix: &Path) -> BTreeMap<String, Entry> {
    let marker = "@PREFIX@";
    let prefix_str = prefix.to_str().expect("prefix is not UTF-8");
    let mut entries = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1) {
        let entry = entry.expect("failed to walk keg");
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let meta = fs::symlink_metadata(entry.path()).expect("failed to stat");
        let mode = meta.permissions().mode() & 0o7777;
        let value = if meta.file_type().is_symlink() {
            let target = fs::read_link(entry.path()).expect("failed to read link");
            Entry::Symlink(target.to_string_lossy().replace(prefix_str, marker))
        } else if meta.is_dir() {
            Entry::Dir
        } else if is_mach_o(entry.path()) {
            Entry::MachO {
                mode,
                load_commands: mach_o_load_commands(entry.path()).replace(prefix_str, marker),
            }
        } else {
            let content = fs::read(entry.path()).expect("failed to read file");
            Entry::File {
                mode,
                content: normalize_prefix(&content, prefix_str, marker),
            }
        };
        entries.insert(rel, value);
    }
    entries
}

fn is_mach_o(path: &Path) -> bool {
    let mut magic = [0_u8; 4];
    std::io::Read::read_exact(
        &mut match fs::File::open(path) {
            Ok(file) => file,
            Err(_) => return false,
        },
        &mut magic,
    )
    .is_ok()
        && matches!(
            magic,
            [0xcf, 0xfa, 0xed, 0xfe] | [0xce, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe]
        )
}

/// Dylib ID, linked libraries and rpaths, as `otool` reports them.
fn mach_o_load_commands(path: &Path) -> String {
    let output = Command::new("otool")
        .args(["-D", "-L", "-l"])
        .arg(path)
        .output()
        .expect("failed to run otool");
    assert!(
        output.status.success(),
        "otool failed on {}",
        path.display()
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = Vec::new();
    let mut in_rpath = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "cmd LC_RPATH" {
            in_rpath = true;
        } else if in_rpath && trimmed.starts_with("path ") {
            lines.push(format!(
                "rpath {}",
                trimmed.split(" (offset").next().unwrap()
            ));
            in_rpath = false;
        } else if trimmed.contains(".dylib") && !trimmed.ends_with(':') {
            lines.push(trimmed.split(" (compatibility").next().unwrap().to_string());
        }
    }
    lines.join("\n")
}

/// Replace `prefix` with `marker`, including the form zerobrew writes into
/// binaries: a prefix shorter than the one the bottle was built in is padded
/// with `/` to the same length, so `/opt/homebrew/Cellar` becomes
/// `/tmp/zbabc////Cellar`. Homebrew pads the same way when it relocates a
/// build prefix.
fn normalize_prefix(content: &[u8], prefix: &str, marker: &str) -> Vec<u8> {
    let build_prefix_len = "/opt/homebrew".len();
    let mut out = content.to_vec();
    if prefix.len() < build_prefix_len {
        let padded = format!("{prefix}{}", "/".repeat(build_prefix_len - prefix.len()));
        out = replace_bytes(&out, padded.as_bytes(), marker.as_bytes());
    }
    replace_bytes(&out, prefix.as_bytes(), marker.as_bytes())
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn single_version_dir(rack: &Path) -> PathBuf {
    let versions: Vec<_> = fs::read_dir(rack)
        .unwrap_or_else(|e| panic!("no rack at {}: {e}", rack.display()))
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .collect();
    assert_eq!(
        versions.len(),
        1,
        "expected exactly one version in {}",
        rack.display()
    );
    versions[0].path()
}

fn stdout(output: &Output, context: &str) -> String {
    assert_success(output, context);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every difference between two snapshots, as human-readable lines.
fn differences(
    reference: &BTreeMap<String, Entry>,
    actual: &BTreeMap<String, Entry>,
    allowlist: &[(&str, &str)],
) -> Vec<String> {
    let allowed = |rel: &str| allowlist.iter().find(|(name, _)| *name == rel);
    let mut diffs = Vec::new();
    for (rel, expected) in reference {
        if allowed(rel).is_some() {
            continue;
        }
        match actual.get(rel) {
            None => diffs.push(format!("missing: {rel}")),
            Some(found) if found != expected => {
                diffs.push(format!(
                    "differs: {rel}\n  homebrew: {}\n  zerobrew: {}",
                    brief(expected),
                    brief(found)
                ));
            }
            Some(_) => {}
        }
    }
    for rel in actual.keys() {
        if !reference.contains_key(rel) && allowed(rel).is_none() {
            diffs.push(format!("extra: {rel}"));
        }
    }
    diffs
}

/// A `Debug` rendering cut to a readable length: a whole static archive in a
/// failure message helps nobody.
fn brief(entry: &Entry) -> String {
    let text = format!("{entry:?}");
    match text.char_indices().nth(200) {
        Some((i, _)) => format!("{}… ({} chars)", &text[..i], text.len()),
        None => text,
    }
}

fn assert_no_differences(what: &str, diffs: &[String]) {
    assert!(
        diffs.is_empty(),
        "{what}: {} difference(s) from Homebrew:\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

/// One formula per kind of bottle the issue asks the harness to cover.
/// `kegs` lists every keg the install produces, dependencies included.
struct Case {
    formula: &'static str,
    kegs: &'static [&'static str],
    /// Bottles built for `/opt/homebrew/Cellar` only pour when the reference
    /// Homebrew lives there; anywhere else it builds from source and the
    /// comparison is meaningless.
    needs_default_prefix: bool,
}

const CASES: &[Case] = &[
    // `:any_skip_relocation`: nothing to patch, the simplest pour.
    Case {
        formula: "hello",
        kegs: &["hello"],
        needs_default_prefix: false,
    },
    // `:any`: dylibs whose install names are rewritten and re-signed.
    Case {
        formula: "xz",
        kegs: &["xz"],
        needs_default_prefix: false,
    },
    // `:any` and keg-only: poured and relocated but not linked.
    Case {
        formula: "readline",
        kegs: &["readline"],
        needs_default_prefix: false,
    },
    // Explicit cellar: Cellar paths baked into the binaries get rewritten.
    Case {
        formula: "pkgconf",
        kegs: &["pkgconf"],
        needs_default_prefix: true,
    },
    // `all` bottle with post-install steps.
    Case {
        formula: "ca-certificates",
        kegs: &["ca-certificates"],
        needs_default_prefix: false,
    },
    // A dependency chain: both kegs must match, not just the named one.
    Case {
        formula: "jq",
        kegs: &["jq", "oniguruma"],
        needs_default_prefix: false,
    },
    // Placeholder rpaths, which `otool -L` does not list: the load commands
    // must still come out like Homebrew's.
    Case {
        formula: "sqlite",
        kegs: &["sqlite"],
        needs_default_prefix: false,
    },
];

fn case(formula: &str) -> &'static Case {
    CASES
        .iter()
        .find(|case| case.formula == formula)
        .unwrap_or_else(|| panic!("no parity case for {formula}"))
}

/// Install with both tools and return `(brew prefix, zb env)` once both are
/// in place, or `None` when the test should skip.
fn install_both(case: &Case) -> Option<(Brew, Zb)> {
    let brew = Brew::find()?;
    if case.needs_default_prefix && brew.prefix != Path::new("/opt/homebrew") {
        eprintln!(
            "skipping {}: explicit-cellar bottles only pour at /opt/homebrew, reference is at {}",
            case.formula,
            brew.prefix.display()
        );
        return None;
    }
    let zb = Zb::new();
    brew.install(case.formula);
    zb.install(case.formula);
    Some((brew, zb))
}

fn reference_keg(brew: &Brew, name: &str) -> PathBuf {
    let opt = PathBuf::from(stdout(
        &brew.run(&["--prefix", name]),
        &format!("brew --prefix {name}"),
    ));
    fs::canonicalize(&opt).unwrap_or_else(|e| panic!("cannot resolve {}: {e}", opt.display()))
}

fn zb_keg(zb: &Zb, name: &str) -> PathBuf {
    fs::canonicalize(single_version_dir(&zb.prefix().join("Cellar").join(name))).unwrap()
}

/// Every keg the install produced matches Homebrew's, and every executable
/// in them starts.
fn check_kegs(case: &Case) {
    let Some((brew, zb)) = install_both(case) else {
        return;
    };
    for name in case.kegs {
        let reference = reference_keg(&brew, name);
        let keg = zb_keg(&zb, name);
        assert_eq!(
            reference.file_name(),
            keg.file_name(),
            "{name}: both installs should pick the same version"
        );
        let diffs = differences(
            &snapshot(&reference, &brew.prefix),
            &snapshot(&keg, &zb.prefix()),
            KEG_ALLOWLIST,
        );
        assert_no_differences(&format!("{name} keg"), &diffs);
        check_executables_start(&keg);
    }
}

/// Relocation and re-signing are only proven by running the result. Every
/// Mach-O executable in `bin` must be accepted by `codesign` and must start:
/// any exit code is fine, dying from a signal (bad signature, missing dylib)
/// is not.
fn check_executables_start(keg: &Path) {
    let bin = keg.join("bin");
    let Ok(entries) = fs::read_dir(&bin) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || !is_mach_o(&path) {
            continue;
        }
        let status = Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(&path)
            .status()
            .expect("failed to run codesign");
        assert!(status.success(), "codesign rejects {}", path.display());

        let output = Command::new(&path)
            .arg("--version")
            .env("PATH", bin.to_string_lossy().as_ref())
            .output()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", path.display()));
        use std::os::unix::process::ExitStatusExt;
        assert!(
            output.status.signal().is_none(),
            "{} died from signal {:?}:\n{}",
            path.display(),
            output.status.signal(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn hello_kegs_match_homebrew() {
    check_kegs(case("hello"));
}

#[test]
fn xz_kegs_match_homebrew() {
    check_kegs(case("xz"));
}

#[test]
fn readline_kegs_match_homebrew() {
    check_kegs(case("readline"));
}

#[test]
fn pkgconf_kegs_match_homebrew() {
    check_kegs(case("pkgconf"));
}

#[test]
fn ca_certificates_kegs_match_homebrew() {
    check_kegs(case("ca-certificates"));
}

#[test]
fn jq_kegs_match_homebrew() {
    check_kegs(case("jq"));
}

#[test]
fn sqlite_kegs_match_homebrew() {
    check_kegs(case("sqlite"));
}

/// The symlinks Homebrew creates in the prefix for a keg, keyed by their path
/// relative to the prefix, with targets relative to the prefix too.
fn links_into(prefix: &Path, keg: &Path) -> BTreeMap<String, String> {
    let mut links = BTreeMap::new();
    for dir in ["bin", "sbin", "lib", "include", "share", "etc", "opt"] {
        let root = prefix.join(dir);
        if !root.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry.expect("failed to walk prefix");
            if !entry.path_is_symlink() {
                continue;
            }
            let Ok(target) = fs::canonicalize(entry.path()) else {
                continue;
            };
            if !target.starts_with(keg) {
                continue;
            }
            let rel = entry.path().strip_prefix(prefix).unwrap();
            let target_rel = target.strip_prefix(keg).unwrap();
            links.insert(
                rel.to_string_lossy().into_owned(),
                format!("<keg>/{}", target_rel.to_string_lossy()),
            );
        }
    }
    links
}

/// The symlinks in the prefix match too, for every keg of every case: which
/// paths are links and where they resolve. Directory links count, so a keg
/// linked file by file where Homebrew links the directory shows up here.
#[test]
fn links_match_homebrew() {
    let mut all_diffs = Vec::new();
    for case in CASES {
        let Some((brew, zb)) = install_both(case) else {
            continue;
        };
        for name in case.kegs {
            let reference = links_into(&brew.prefix, &reference_keg(&brew, name));
            let actual = links_into(&zb.prefix(), &zb_keg(&zb, name));
            for (rel, target) in &reference {
                match actual.get(rel) {
                    None => all_diffs.push(format!("{name}: missing link: {rel} -> {target}")),
                    Some(found) if found != target => all_diffs.push(format!(
                        "{name}: link differs: {rel}\n  homebrew: {target}\n  zerobrew: {found}"
                    )),
                    Some(_) => {}
                }
            }
            for rel in actual.keys() {
                if !reference.contains_key(rel) {
                    all_diffs.push(format!("{name}: extra link: {rel}"));
                }
            }
        }
    }
    assert_no_differences("links", &all_diffs);
}

/// How much faster than Homebrew `zb install` has to be, per case, as a
/// ratio of wall-clock seconds. Read from `ZB_TIMING_MIN_SPEEDUP`; unset
/// means report only. Hosted runners vary too much run to run for absolute
/// times to mean anything, so only the ratio measured in one run is gated.
fn min_speedup() -> Option<f64> {
    let raw = std::env::var("ZB_TIMING_MIN_SPEEDUP").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(
        raw.trim()
            .parse()
            .unwrap_or_else(|e| panic!("ZB_TIMING_MIN_SPEEDUP={raw:?} is not a number: {e}")),
    )
}

/// How much slower than the last release `zb install` may be, per case, as
/// a ratio. Read from `ZB_TIMING_MAX_REGRESSION`; unset means report only.
/// Only checked when `ZB_TIMING_BASELINE` names a released `zb`.
fn max_regression() -> Option<f64> {
    let raw = std::env::var("ZB_TIMING_MAX_REGRESSION").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(
        raw.trim()
            .parse()
            .unwrap_or_else(|e| panic!("ZB_TIMING_MAX_REGRESSION={raw:?} is not a number: {e}")),
    )
}

/// A released `zb` to time alongside the one under test, named by
/// `ZB_TIMING_BASELINE`. Comparing two zb builds in the same run is the only
/// way to see a regression on hosted runners, whose speed varies run to run.
fn baseline_zb() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("ZB_TIMING_BASELINE")?);
    assert!(
        path.is_file(),
        "ZB_TIMING_BASELINE={} is not a file",
        path.display()
    );
    Some(path)
}

/// How many times each zb build installs each case; the fastest run counts.
const ZB_TIMING_ROUNDS: usize = 3;

struct Timing {
    formula: &'static str,
    brew: Duration,
    zb: Duration,
    /// The last release's time, when a baseline was given.
    baseline: Option<Duration>,
}

impl Timing {
    fn speedup(&self) -> f64 {
        self.brew.as_secs_f64() / self.zb.as_secs_f64()
    }

    /// Current time over the last release's: above 1 means slower.
    fn regression(&self) -> Option<f64> {
        self.baseline
            .map(|baseline| self.zb.as_secs_f64() / baseline.as_secs_f64())
    }
}

/// Make Homebrew forget `case` so its install downloads and pours like a
/// first install: remove the kegs the runner may ship preinstalled and the
/// bottles in its download cache.
fn make_brew_cold(brew: &Brew, case: &Case) {
    let installed: Vec<&str> = case
        .kegs
        .iter()
        .copied()
        .filter(|keg| {
            brew.run(&["list", "--formula", "--versions", keg])
                .status
                .success()
        })
        .collect();
    if !installed.is_empty() {
        let mut args = vec!["uninstall", "--formula", "--ignore-dependencies", "--force"];
        args.extend(installed);
        assert_success(&brew.run(&args), "brew uninstall");
    }
    let cache = PathBuf::from(stdout(&brew.run(&["--cache"]), "brew --cache")).join("downloads");
    if let Ok(entries) = fs::read_dir(&cache) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Download names look like `<sha>--<formula>--<version>.<tag>.bottle.tar.gz`
            // and `<sha>--<formula>-<version>.bottle_manifest.json`.
            if case
                .kegs
                .iter()
                .any(|keg| name.contains(&format!("--{keg}-")))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

fn timing_table(timings: &[Timing]) -> String {
    let with_baseline = timings.iter().any(|t| t.baseline.is_some());
    let mut table = String::from("| formula | homebrew | zerobrew | speedup |");
    if with_baseline {
        table.push_str(" last release | vs release |");
    }
    table.push_str("\n|---|---:|---:|---:|");
    if with_baseline {
        table.push_str("---:|---:|");
    }
    table.push('\n');
    for t in timings {
        table.push_str(&format!(
            "| {} | {:.2}s | {:.2}s | {:.1}x |",
            t.formula,
            t.brew.as_secs_f64(),
            t.zb.as_secs_f64(),
            t.speedup()
        ));
        if with_baseline {
            match (t.baseline, t.regression()) {
                (Some(baseline), Some(ratio)) => table.push_str(&format!(
                    " {:.2}s | {:+.0}% |",
                    baseline.as_secs_f64(),
                    (ratio - 1.0) * 100.0
                )),
                _ => table.push_str(" - | - |"),
            }
        }
        table.push('\n');
    }
    table
}

/// Install every case cold with both tools and compare wall-clock time.
///
/// Runs in its own CI job on a fresh runner, so nothing is cached for either
/// tool. `brew update` runs once beforehand and is not timed: zb has no
/// equivalent step, and the API refresh is the part of `brew install` that
/// depends most on the network. See
/// https://github.com/zerobrewhq/zerobrew/issues/422
#[test]
fn install_timings() {
    let Some(brew) = Brew::find() else {
        return;
    };
    if std::env::var_os("ZB_PARITY_BREW").is_none() {
        assert_success(&brew.run(&["update", "--quiet"]), "brew update");
    }

    let mut timings = Vec::new();
    for case in CASES {
        if case.needs_default_prefix && brew.prefix != Path::new("/opt/homebrew") {
            continue;
        }
        make_brew_cold(&brew, case);
        let (_, brew_time) = timed(|| {
            assert_success(
                &brew.run(&["install", "--formula", case.formula]),
                &format!("brew install {}", case.formula),
            )
        });
        // zb installs are short enough to repeat. Each round installs into a
        // fresh root with the build under test and the release in alternating
        // order, so neither always gets the warmer CDN edge, and the fastest
        // run counts: the slowest ones are the network, not the tool.
        let release = baseline_zb();
        let mut zb_times = Vec::new();
        let mut baseline_times = Vec::new();
        for round in 0..ZB_TIMING_ROUNDS {
            let time_current = || timed(|| Zb::new().install(case.formula)).1;
            let time_release =
                |bin: &PathBuf| timed(|| Zb::with_binary(bin.clone()).install(case.formula)).1;
            match &release {
                Some(bin) if round % 2 == 1 => {
                    baseline_times.push(time_release(bin));
                    zb_times.push(time_current());
                }
                Some(bin) => {
                    zb_times.push(time_current());
                    baseline_times.push(time_release(bin));
                }
                None => zb_times.push(time_current()),
            }
        }
        timings.push(Timing {
            formula: case.formula,
            brew: brew_time,
            zb: zb_times.into_iter().min().unwrap(),
            baseline: baseline_times.into_iter().min(),
        });
    }

    let table = timing_table(&timings);
    eprintln!("\n{table}");
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(summary)
            .expect("cannot open GITHUB_STEP_SUMMARY");
        writeln!(file, "### install timings, cold\n\n{table}").expect("cannot write summary");
    }
    // For the workflow to post on the pull request.
    if let Some(path) = std::env::var_os("ZB_TIMING_TABLE") {
        fs::write(path, &table).expect("cannot write ZB_TIMING_TABLE");
    }

    if let Some(min) = min_speedup() {
        let slow: Vec<String> = timings
            .iter()
            .filter(|t| t.speedup() < min)
            .map(|t| format!("{}: {:.1}x (need {min}x)", t.formula, t.speedup()))
            .collect();
        assert!(
            slow.is_empty(),
            "zb install is not {min}x faster than Homebrew for:\n{}",
            slow.join("\n")
        );
    }

    if let Some(max) = max_regression() {
        // One number for the whole set: a real slowdown shows up in every
        // case, a network hiccup in one.
        let (current, release): (f64, f64) = timings
            .iter()
            .filter_map(|t| t.baseline.map(|b| (t.zb.as_secs_f64(), b.as_secs_f64())))
            .fold((0.0, 0.0), |(c, r), (zb, b)| (c + zb, r + b));
        if release > 0.0 {
            let ratio = current / release;
            assert!(
                ratio <= max,
                "zb install is {:+.0}% slower than the last release over all cases (limit {:+.0}%)\n{table}",
                (ratio - 1.0) * 100.0,
                (max - 1.0) * 100.0
            );
        }
    }
}
