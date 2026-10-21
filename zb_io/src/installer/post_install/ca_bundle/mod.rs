//! The CA bundle ca-certificates builds in post-install, built in process
//! and cached.
//!
//! The formula's step runs `libexec/post-install SOURCE DESTINATION`, a
//! shell script that exports the system keychain, checks every certificate
//! with `openssl` and `security` one process at a time and merges in the
//! Mozilla bundle. That is several hundred processes and about thirteen
//! seconds on macOS. Its output depends only on the Mozilla bundle it is
//! given, the system trust store and the script itself.
//!
//! When the script is one this module knows ([`KNOWN_SCRIPTS`]), the bundle
//! is built here instead, following the script line for line: [`pem`]
//! splits and reads certificates as its `awk` and `openssl` do, [`purpose`]
//! answers the `openssl x509` questions it asks, and on macOS [`macos`]
//! asks Security.framework what `security verify-cert` asks it. The bytes
//! come out identical; the tests check that against the script. Any other
//! version of the script runs unchanged, so a change upstream costs the
//! speed, never the result.
//!
//! Either way the result is kept, keyed on everything it depends on, so
//! reinstalling ca-certificates, or anything that depends on it, copies the
//! bundle instead of building it again.

mod pem;
// Only the keychain checks on macOS ask these questions; the tests ask them everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod purpose;

#[cfg(target_os = "macos")]
mod macos;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use tracing::warn;

use self::pem::{Bundle, hex};
use super::steps::write_atomically;
use super::{PostInstallContext, StepError};

/// A `libexec/post-install` this module reproduces, by SHA-256, and what
/// it does that an earlier one did not. The script is written by the
/// formula's `install`; a new hash means reading its diff against the last
/// fixture, extending the port if the behaviour changed, and adding a row
/// here and a fixture under `fixtures/ca-bundle`.
struct KnownScript {
    hash: &'static str,
    /// The keys its `append_unique_certificate` drops, if any.
    excluded_keys: &'static [&'static str],
}

/// The SubjectPublicKeyInfo hashes the 2026-10-20 script excludes: AAA
/// Certificate Services (Comodo) and the two emSign G3 roots.
pub(super) const EXCLUDED_KEYS_2026_10_20: &[&str] = &[
    "bd153ed7b0434f6886b17bce8bbe84ed340c7132d702a8f4fa318f756ecbd6f3",
    "c619e4780ed8e0abeda24ad732567de4383a7ea941338f6d1a7d1b49ac792fe9",
    "8d417db2dd8bf5e3084d1e3f196d583849d81bdd4c00c70b9d39369e96b8c782",
];

const KNOWN_SCRIPTS: &[KnownScript] = &[
    // ca-certificates 2026-09-25 (homebrew-core a48f632d).
    KnownScript {
        hash: "0d382c7231f5f0378947729c5815f7fb8f6c12c16396172513109b13bd59b900",
        excluded_keys: &[],
    },
    // ca-certificates 2026-10-20: drops three roots by key.
    KnownScript {
        hash: "da9391b40cabf30802bab7f27cfbdd4292bbc79155a4731e671526351ee24825",
        excluded_keys: EXCLUDED_KEYS_2026_10_20,
    },
];

fn known_script(hash: &str) -> Option<&'static KnownScript> {
    KNOWN_SCRIPTS.iter().find(|script| script.hash == hash)
}

/// Whether `script` is a `libexec/post-install` this module builds in
/// process. The parity harness checks the installed bottle's, so an
/// upstream change shows up as a failing nightly rather than as the script
/// quietly running again.
pub fn known_ca_bundle_script(script: &Path) -> bool {
    fs::read(script)
        .ok()
        .and_then(|content| known_script(&hex(&Sha256::digest(content))))
        .is_some()
}

/// The system trust stores the script reads, in the order it reads them.
#[cfg(target_os = "macos")]
const TRUST_SOURCES: &[&str] = &[
    "/Library/Keychains/System.keychain",
    "/System/Library/Keychains/SystemRootCertificates.keychain",
];

#[cfg(not(target_os = "macos"))]
const TRUST_SOURCES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/ca-bundle.pem",
];

pub(super) struct CaBundle {
    script: PathBuf,
    script_hash: Option<String>,
    known: Option<&'static KnownScript>,
    source: PathBuf,
    destination: PathBuf,
    cached: PathBuf,
}

impl CaBundle {
    /// Recognise ca-certificates' bundle step and work out its cache key.
    /// `None` for any other command, and for a step that overrides the
    /// environment: neither the key nor the in-process build accounts for
    /// what the script might do differently under it.
    pub(super) fn detect(
        ctx: &PostInstallContext,
        program: &OsStr,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Option<Self> {
        if ctx.name != "ca-certificates" || args.len() != 2 || !env.is_empty() {
            return None;
        }
        let script = Path::new(program);
        if script.file_name() != Some(OsStr::new("post-install")) {
            return None;
        }
        let mozilla = fs::read(&args[0]).ok()?;
        let script_hash = fs::read(script)
            .ok()
            .map(|content| hex(&Sha256::digest(content)));

        let mut hasher = Sha256::new();
        hasher.update(&mozilla);
        hasher.update(script_hash.as_deref().unwrap_or("unreadable script"));
        for source in TRUST_SOURCES {
            hasher.update(source.as_bytes());
            match fs::metadata(source) {
                Ok(metadata) => {
                    hasher.update(metadata.len().to_le_bytes());
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|age| age.as_nanos())
                        .unwrap_or(0);
                    hasher.update(modified.to_le_bytes());
                }
                Err(_) => hasher.update(b"missing"),
            }
        }
        #[cfg(target_os = "macos")]
        hasher.update(zb_core::macos_major_version().unwrap_or(0).to_le_bytes());
        let key = hex(&hasher.finalize());

        Some(Self {
            script: script.to_path_buf(),
            known: script_hash.as_deref().and_then(known_script),
            script_hash,
            source: PathBuf::from(&args[0]),
            destination: PathBuf::from(&args[1]),
            cached: ctx.cache.join("ca-certificates").join(format!("{key}.pem")),
        })
    }

    /// Put the cached bundle in place. `false` when there is none.
    pub(super) fn restore(&self) -> Result<bool, StepError> {
        let Ok(content) = fs::read(&self.cached) else {
            return Ok(false);
        };
        self.write(&content)?;
        Ok(true)
    }

    /// Build the bundle in process and put it in place. `false` when the
    /// script is not one this module reproduces, or the build failed: the
    /// script then runs as it would have.
    pub(super) fn build(&self, ctx: &PostInstallContext) -> Result<bool, StepError> {
        let Some(known) = self.known else {
            warn!(
                script = %self.script.display(),
                hash = self.script_hash.as_deref().unwrap_or("unreadable"),
                "ca-certificates' post-install script is not one zerobrew knows; running it instead of building the bundle in process"
            );
            return Ok(false);
        };
        let content = match self.build_bytes(ctx, known) {
            Ok(content) => content,
            Err(error) => {
                warn!(%error, "cannot build the CA bundle in process; running the script");
                return Ok(false);
            }
        };
        self.write(&content)?;
        Ok(true)
    }

    /// Remember the bundle that was just written.
    pub(super) fn store(&self) -> Result<(), StepError> {
        let Ok(content) = fs::read(&self.destination) else {
            return Ok(());
        };
        if let Some(parent) = self.cached.parent() {
            fs::create_dir_all(parent).map_err(StepError::io("creating cache directory"))?;
        }
        write_atomically(&self.cached, &content)
    }

    /// `mkdir -p`, `mv -f` and `chmod 0644`, as the script ends.
    fn write(&self, content: &[u8]) -> Result<(), StepError> {
        if let Some(parent) = self.destination.parent() {
            fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
        }
        write_atomically(&self.destination, content)?;
        fs::set_permissions(&self.destination, fs::Permissions::from_mode(0o644))
            .map_err(StepError::io("setting bundle mode"))
    }

    /// The bytes `known` would write.
    fn build_bytes(
        &self,
        ctx: &PostInstallContext,
        known: &KnownScript,
    ) -> Result<Vec<u8>, StepError> {
        let mozilla =
            fs::read(&self.source).map_err(StepError::io("reading the Mozilla bundle"))?;
        let mut bundle = Bundle::new(known.excluded_keys);
        append_system_certificates(ctx, &mut bundle, &mozilla)?;
        Ok(bundle.into_bytes())
    }
}

/// macOS: system trust first, then the Mozilla certificates not already
/// present. The keychains are checked under the policies the script names.
#[cfg(target_os = "macos")]
fn append_system_certificates(
    _ctx: &PostInstallContext,
    bundle: &mut Bundle,
    mozilla: &[u8],
) -> Result<(), StepError> {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|age| age.as_secs() as i64)
        .unwrap_or(0);
    append_keychain(bundle, TRUST_SOURCES[0], macos::Policy::Ssl, now)?;
    append_keychain(bundle, TRUST_SOURCES[1], macos::Policy::Basic, now)?;
    bundle
        .append_bundle(mozilla)
        .map_err(|pem::Unreadable| unreadable("the Mozilla bundle"))?;
    Ok(())
}

/// The script's `append_keychain`: every unexpired certificate in
/// `keychain` that `openssl` calls an SSL server CA and `security
/// verify-cert` trusts under `policy`, once each.
#[cfg(target_os = "macos")]
fn append_keychain(
    bundle: &mut Bundle,
    keychain: &str,
    policy: macos::Policy,
    now: i64,
) -> Result<(), StepError> {
    let exported = macos::export_keychain(keychain)?;
    let chunks = pem::certificates(&exported);
    let mut candidates: Vec<pem::Certificate> = Vec::new();
    for chunk in &chunks {
        let Some(certificate) = pem::Certificate::parse(chunk) else {
            if pem::surely_rejected(chunk) {
                continue;
            }
            return Err(unreadable(keychain));
        };
        let x509 = certificate.x509();
        if purpose::fails_checkend(&x509, now) {
            continue;
        }
        match purpose::is_ssl_server_ca(&x509) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(purpose::Undecided) => {
                return Err(StepError::Failed(format!(
                    "cannot tell whether {} is an SSL server CA",
                    x509.subject()
                )));
            }
        }
        candidates.push(certificate);
    }
    let ders: Vec<&[u8]> = candidates.iter().map(|c| c.der.as_slice()).collect();
    for (certificate, trusted) in candidates.iter().zip(macos::trusted(&ders, policy)) {
        if trusted {
            bundle.append_unique(certificate);
        }
    }
    Ok(())
}

/// Linux: the first standard system bundle merged with the Mozilla bundle,
/// or the Mozilla certificates alone, unchecked, when there is no system
/// bundle or no `openssl` for the script to check them with.
#[cfg(not(target_os = "macos"))]
fn append_system_certificates(
    ctx: &PostInstallContext,
    bundle: &mut Bundle,
    mozilla: &[u8],
) -> Result<(), StepError> {
    // `[[ -f "$candidate" && -r "$candidate" ]]`
    let system = TRUST_SOURCES
        .iter()
        .map(Path::new)
        .find(|candidate| candidate.is_file() && fs::File::open(candidate).is_ok());
    // `command -v openssl`, on the PATH the script runs with.
    let openssl = std::env::split_paths(&super::steps::command_path(ctx))
        .map(|dir| dir.join("openssl"))
        .any(|candidate| {
            fs::metadata(&candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        });
    match system {
        Some(system) if openssl => {
            let content =
                fs::read(system).map_err(StepError::io("reading the system CA bundle"))?;
            bundle
                .append_bundle(&content)
                .map_err(|pem::Unreadable| unreadable(&system.display().to_string()))?;
            bundle
                .append_bundle(mozilla)
                .map_err(|pem::Unreadable| unreadable("the Mozilla bundle"))?;
        }
        _ => {
            warn!(
                "cannot find a readable system CA bundle or OpenSSL; using Mozilla certificates only"
            );
            for chunk in pem::certificates(mozilla) {
                bundle.append_raw(&chunk);
            }
        }
    }
    Ok(())
}

/// The error for a certificate in `source` that `openssl` might read and
/// this module cannot: the build stops and the script runs.
fn unreadable(source: &str) -> StepError {
    StepError::Failed(format!(
        "a certificate in {source} that openssl may read could not be parsed"
    ))
}

#[cfg(test)]
mod tests {
    use super::super::steps::{command_env, run_step};
    use super::super::test_support::context;
    use super::pem::tests::{
        AAA_CERTIFICATE_SERVICES, EMSIGN_ECC_ROOT_G3, awkward_bundle, fixture, which,
    };
    use super::*;
    use std::collections::BTreeMap;
    use std::process::Command;
    use tempfile::TempDir;
    use zb_core::{PathBase, PathSpec, StepAction, Sudo};

    /// The fixtures, one per row of [`KNOWN_SCRIPTS`], in the same order.
    const SCRIPTS: &[&str] = &["post-install-0d382c72.sh", "post-install-da9391b4.sh"];

    /// A stand-in for the real script: counts its runs and writes the
    /// source bundle to the destination with a marker.
    fn install_script(ctx: &PostInstallContext) -> PathBuf {
        let script = ctx.keg.join("libexec/post-install");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(
            &script,
            "#!/bin/sh\necho run >> \"$HOMEBREW_PREFIX/runs\"\nmkdir -p \"$(dirname \"$2\")\"\n(cat \"$1\"; echo built) > \"$2\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// A real script, as a bottle ships it.
    fn install_real_script(ctx: &PostInstallContext, name: &str) -> PathBuf {
        let script = ctx.keg.join("libexec/post-install");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::copy(fixture(name), &script).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The awkward bundle plus two roots the newer script drops by key.
    fn bundle_with_excluded_roots() -> Vec<u8> {
        let mut bundle = awkward_bundle();
        bundle.extend_from_slice(b"\nAAA Certificate Services\n");
        bundle.extend_from_slice(AAA_CERTIFICATE_SERVICES);
        bundle.extend_from_slice(EMSIGN_ECC_ROOT_G3);
        bundle
    }

    fn bundle_step() -> StepAction {
        StepAction::Run {
            command: PathSpec::based(PathBase::Libexec, "post-install"),
            args: vec![
                "{{pkgshare}}/cacert.pem".into(),
                "{{pkgetc}}/cert.pem".into(),
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

    fn mozilla_source(ctx: &PostInstallContext, content: &[u8]) -> PathBuf {
        let source = ctx.keg.join("share/ca-certificates/cacert.pem");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, content).unwrap();
        source
    }

    #[test]
    fn the_known_scripts_are_the_fixtures() {
        assert_eq!(SCRIPTS.len(), KNOWN_SCRIPTS.len());
        for (name, known) in SCRIPTS.iter().zip(KNOWN_SCRIPTS) {
            let content = fs::read(fixture(name)).unwrap();
            let hash = hex(&Sha256::digest(&content));
            assert_eq!(known.hash, hash, "{name} hashes to {hash}");
            assert!(name.contains(&hash[..8]));
            assert!(known_ca_bundle_script(&fixture(name)));
        }
        assert!(!known_ca_bundle_script(&fixture("root-ca.pem")));
        assert!(!known_ca_bundle_script(Path::new("/nonexistent")));
    }

    /// The reason this module may exist: on the same inputs, the bundle
    /// built here is the one the script writes, byte for byte. On macOS
    /// that covers the keychain export and the trust checks; on Linux the
    /// system bundle and the `openssl` fallback.
    #[test]
    fn builds_the_same_bytes_as_the_script() {
        let (Ok(bash), Ok(_awk)) = (which("bash"), which("awk")) else {
            eprintln!("skipping the script comparison: bash or awk not found");
            return;
        };
        let mut outputs = Vec::new();
        for name in SCRIPTS {
            let tmp = TempDir::new().unwrap();
            let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
            let script = install_real_script(&ctx, name);
            let source = mozilla_source(&ctx, &bundle_with_excluded_roots());
            let expected = tmp.path().join("script/cert.pem");
            let output = Command::new(&bash)
                .arg(&script)
                .arg(&source)
                .arg(&expected)
                .env_clear()
                .envs(command_env(&ctx, &ctx.temp))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{name} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let expected = fs::read(expected).unwrap();
            assert!(!pem::split_certificates(&expected).is_empty());

            let args = [
                source.display().to_string(),
                tmp.path().join("native/cert.pem").display().to_string(),
            ];
            let bundle =
                CaBundle::detect(&ctx, script.as_os_str(), &args, &BTreeMap::new()).unwrap();
            let known = bundle.known.expect("the fixture is a known script");
            let native = bundle.build_bytes(&ctx, known).unwrap();
            assert!(
                native == expected,
                "the in-process bundle differs from {name}'s: {}",
                describe_difference(&native, &expected)
            );
            outputs.push(expected);
        }
        // The two scripts do differ on this input: the newer one drops the
        // excluded roots, so matching both is not matching by accident.
        assert_ne!(outputs[0], outputs[1]);
        assert_eq!(
            pem::split_certificates(&outputs[0]).len(),
            pem::split_certificates(&outputs[1]).len() + 2
        );
    }

    /// Where two bundles differ: the sizes, the first certificate that
    /// differs, and the certificates only one side has.
    fn describe_difference(ours: &[u8], theirs: &[u8]) -> String {
        let subject = |chunk: &[u8]| {
            pem::Certificate::parse(chunk)
                .map(|c| c.x509().subject().to_string())
                .unwrap_or_else(|| "<unreadable>".to_string())
        };
        let ours_chunks = pem::split_certificates(ours);
        let theirs_chunks = pem::split_certificates(theirs);
        let mut report = format!(
            "{} vs {} bytes, {} vs {} certificates\n",
            ours.len(),
            theirs.len(),
            ours_chunks.len(),
            theirs_chunks.len()
        );
        if let Some((index, (a, b))) = ours_chunks
            .iter()
            .zip(&theirs_chunks)
            .enumerate()
            .find(|(_, (a, b))| a != b)
        {
            report += &format!(
                "first difference at certificate {index}: ours {} bytes, {}; theirs {} bytes, {}\n",
                a.len(),
                subject(a),
                b.len(),
                subject(b)
            );
        }
        let fingerprints = |chunks: &[Vec<u8>]| -> std::collections::HashMap<[u8; 32], Vec<u8>> {
            chunks
                .iter()
                .filter_map(|c| pem::Certificate::parse(c).map(|p| (p.fingerprint, c.clone())))
                .collect()
        };
        let (ours_set, theirs_set) = (fingerprints(&ours_chunks), fingerprints(&theirs_chunks));
        for (name, mine, other) in [
            ("ours", &ours_set, &theirs_set),
            ("theirs", &theirs_set, &ours_set),
        ] {
            for (fingerprint, chunk) in mine {
                if !other.contains_key(fingerprint) {
                    report += &format!("only in {name}: {}\n{}", subject(chunk), probe(chunk));
                }
            }
        }
        report
    }

    /// What each of the script's checks says about one certificate, next
    /// to what this module says, so a disagreement names its cause.
    #[cfg(target_os = "macos")]
    fn probe(chunk: &[u8]) -> String {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("cert.pem");
        fs::write(&file, chunk).unwrap();
        let run = |program: &str, args: &[&str]| {
            let output = Command::new(program)
                .args(args)
                .arg(&file)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stdout = stdout
                .lines()
                .filter(|line| !line.contains(" : ") || line.starts_with("SSL server CA"))
                .collect::<Vec<_>>()
                .join(" | ");
            format!(
                "  {program} {}: exit {:?}; {stdout} {}\n",
                args.join(" "),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )
        };
        let mut report = String::new();
        report += &run(
            "/usr/bin/openssl",
            &["x509", "-inform", "pem", "-checkend", "0", "-noout", "-in"],
        );
        report += &run(
            "/usr/bin/openssl",
            &["x509", "-inform", "pem", "-purpose", "-noout", "-in"],
        );
        for policy in ["ssl", "basic"] {
            report += &run(
                "/usr/bin/security",
                &[
                    "verify-cert",
                    "-l",
                    "-L",
                    "-p",
                    policy,
                    "-R",
                    "offline",
                    "-c",
                ],
            );
        }
        let certificate = pem::Certificate::parse(chunk).unwrap();
        let x509 = certificate.x509();
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        report += &format!(
            "  native: expired={} ssl_server_ca={:?} trusted(ssl)={:?} trusted(basic)={:?} der={} bytes\n",
            purpose::fails_checkend(&x509, now),
            purpose::is_ssl_server_ca(&x509),
            macos::trusted(&[certificate.der.as_slice()], macos::Policy::Ssl),
            macos::trusted(&[certificate.der.as_slice()], macos::Policy::Basic),
            certificate.der.len()
        );
        report
    }

    #[cfg(not(target_os = "macos"))]
    fn probe(_chunk: &[u8]) -> String {
        String::new()
    }

    #[test]
    fn a_known_script_is_built_in_process_and_cached() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        install_real_script(&ctx, SCRIPTS[0]);
        mozilla_source(&ctx, &awkward_bundle());
        let destination = ctx.prefix.join("etc/ca-certificates/cert.pem");

        run_step(&ctx, &bundle_step()).unwrap();
        let built = fs::read(&destination).unwrap();
        assert!(built.ends_with(b"-----END CERTIFICATE-----\n\n"));
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let cached: Vec<_> = fs::read_dir(ctx.cache.join("ca-certificates"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(cached.len(), 1);
        assert_eq!(fs::read(&cached[0]).unwrap(), built);
    }

    #[test]
    fn the_script_is_part_of_the_cache_key() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        let script = install_script(&ctx);
        let source = mozilla_source(&ctx, b"MOZILLA\n");
        let args = [source.display().to_string(), "b".to_string()];
        let first = CaBundle::detect(&ctx, script.as_os_str(), &args, &BTreeMap::new()).unwrap();
        fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        let second = CaBundle::detect(&ctx, script.as_os_str(), &args, &BTreeMap::new()).unwrap();
        assert_ne!(first.cached, second.cached);
    }

    #[test]
    fn an_unknown_script_runs_and_is_cached() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        install_script(&ctx);
        let source = mozilla_source(&ctx, b"MOZILLA\n");
        let destination = ctx.prefix.join("etc/ca-certificates/cert.pem");

        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 1);
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "MOZILLA\nbuilt\n"
        );

        fs::remove_file(&destination).unwrap();
        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 1, "the second run comes from the cache");
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "MOZILLA\nbuilt\n"
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // A new Mozilla bundle is a new key.
        fs::write(&source, "MOZILLA v2\n").unwrap();
        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 2);
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "MOZILLA v2\nbuilt\n"
        );
    }

    #[test]
    fn a_step_with_env_overrides_runs_the_script_every_time() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        install_script(&ctx);
        mozilla_source(&ctx, b"MOZILLA\n");
        let mut step = bundle_step();
        if let StepAction::Run { env, .. } = &mut step {
            env.insert("PATH".into(), "/usr/bin:/bin".into());
        }

        run_step(&ctx, &step).unwrap();
        run_step(&ctx, &step).unwrap();
        assert_eq!(runs(&ctx), 2, "the script runs each time");
        assert!(!ctx.cache.exists(), "and nothing is cached");
    }

    #[test]
    fn only_ca_certificates_is_cached() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "other-certs", "1");
        install_script(&ctx);
        let source = ctx.keg.join("share/other-certs/cacert.pem");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "X\n").unwrap();

        run_step(&ctx, &bundle_step()).unwrap();
        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 2);
        assert!(!ctx.cache.exists());
    }

    #[test]
    fn detect_ignores_other_commands_and_missing_sources() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "1");
        let no_env = BTreeMap::new();
        let args = ["a".to_string(), "b".to_string()];
        assert!(CaBundle::detect(&ctx, OsStr::new("/bin/sh"), &args, &no_env).is_none());
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args, &no_env).is_none());
        let source = ctx.keg.join("cacert.pem");
        fs::write(&source, "M").unwrap();
        let args = [source.display().to_string(), "b".to_string()];
        let bundle = CaBundle::detect(&ctx, OsStr::new("post-install"), &args, &no_env).unwrap();
        assert!(
            bundle.known.is_none() && bundle.script_hash.is_none(),
            "an unreadable script is not a known one"
        );
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args[..1], &no_env).is_none());
        let env = BTreeMap::from([("PATH".to_string(), "/nowhere".to_string())]);
        assert!(
            CaBundle::detect(&ctx, OsStr::new("post-install"), &args, &env).is_none(),
            "a step with environment overrides is left to the script"
        );
    }
}
