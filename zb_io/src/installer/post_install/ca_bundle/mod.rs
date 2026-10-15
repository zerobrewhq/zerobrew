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

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use self::pem::Bundle;
use super::steps::write_atomically;
use super::{PostInstallContext, StepError};

/// SHA-256 of each `libexec/post-install` this module reproduces. The
/// script is written by the formula's `install`; a new hash means reading
/// its diff, updating the port if it changed, and adding the hash.
///
/// - `0d382c72`: Formula/c/ca-certificates.rb as of 2026-09-25
///   (homebrew-core a48f632d)
const KNOWN_SCRIPTS: &[&str] =
    &["0d382c7231f5f0378947729c5815f7fb8f6c12c16396172513109b13bd59b900"];

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
    source: PathBuf,
    destination: PathBuf,
    cached: PathBuf,
    script_is_known: bool,
}

impl CaBundle {
    /// Recognise ca-certificates' bundle step and work out its cache key.
    /// `None` for any other command.
    pub(super) fn detect(
        ctx: &PostInstallContext,
        program: &OsStr,
        args: &[String],
    ) -> Option<Self> {
        if ctx.name != "ca-certificates" || args.len() != 2 {
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
            source: PathBuf::from(&args[0]),
            destination: PathBuf::from(&args[1]),
            cached: ctx.cache.join("ca-certificates").join(format!("{key}.pem")),
            script_is_known: script_hash.is_some_and(|hash| KNOWN_SCRIPTS.contains(&hash.as_str())),
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
        if !self.script_is_known {
            debug!(script = %self.script.display(), "unknown post-install script; running it");
            return Ok(false);
        }
        let content = match self.build_bytes(ctx) {
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

    /// The bytes the script would write.
    fn build_bytes(&self, ctx: &PostInstallContext) -> Result<Vec<u8>, StepError> {
        let mozilla =
            fs::read(&self.source).map_err(StepError::io("reading the Mozilla bundle"))?;
        let mut bundle = Bundle::default();
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
    bundle.append_bundle(mozilla);
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
    let candidates: Vec<pem::Certificate> = chunks
        .iter()
        .filter_map(|chunk| pem::Certificate::parse(chunk))
        .filter(|certificate| {
            let x509 = certificate.x509();
            !purpose::is_expired(&x509, now) && purpose::is_ssl_server_ca(&x509)
        })
        .collect();
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
            let system = fs::read(system).map_err(StepError::io("reading the system CA bundle"))?;
            bundle.append_bundle(&system);
            bundle.append_bundle(mozilla);
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

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::super::steps::{command_env, run_step};
    use super::super::test_support::context;
    use super::pem::tests::{awkward_bundle, fixture, which};
    use super::*;
    use std::collections::BTreeMap;
    use std::process::Command;
    use tempfile::TempDir;
    use zb_core::{PathBase, PathSpec, StepAction, Sudo};

    const SCRIPT: &str = "post-install-0d382c72.sh";

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

    /// The real script, as the bottle ships it.
    fn install_real_script(ctx: &PostInstallContext) -> PathBuf {
        let script = ctx.keg.join("libexec/post-install");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::copy(fixture(SCRIPT), &script).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
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
        let content = fs::read(fixture(SCRIPT)).unwrap();
        let hash = hex(&Sha256::digest(&content));
        assert!(
            KNOWN_SCRIPTS.contains(&hash.as_str()),
            "{SCRIPT} hashes to {hash}"
        );
        assert!(SCRIPT.contains(&hash[..8]));
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
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        let script = install_real_script(&ctx);
        let source = mozilla_source(&ctx, &awkward_bundle());
        let expected = tmp.path().join("script/cert.pem");
        let output = Command::new(bash)
            .arg(&script)
            .arg(&source)
            .arg(&expected)
            .env_clear()
            .envs(command_env(&ctx, &ctx.temp))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = fs::read(expected).unwrap();
        assert!(!pem::split_certificates(&expected).is_empty());

        let args = [
            source.display().to_string(),
            tmp.path().join("native/cert.pem").display().to_string(),
        ];
        let bundle = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
        assert!(bundle.script_is_known);
        let native = bundle.build_bytes(&ctx).unwrap();
        if native != expected {
            let dump = std::env::temp_dir().join("zb-ca-bundle");
            fs::create_dir_all(&dump).unwrap();
            fs::write(dump.join("script.pem"), &expected).unwrap();
            fs::write(dump.join("native.pem"), &native).unwrap();
            fs::copy(&source, dump.join("mozilla.pem")).unwrap();
            eprintln!("both bundles written under {}", dump.display());
        }
        assert!(
            native == expected,
            "the in-process bundle differs from the script's: {} vs {} bytes, {} vs {} certificates",
            native.len(),
            expected.len(),
            pem::split_certificates(&native).len(),
            pem::split_certificates(&expected).len()
        );
    }

    #[test]
    fn a_known_script_is_built_in_process_and_cached() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        install_real_script(&ctx);
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
        let first = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
        fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        let second = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
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
        let args = ["a".to_string(), "b".to_string()];
        assert!(CaBundle::detect(&ctx, OsStr::new("/bin/sh"), &args).is_none());
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args).is_none());
        let source = ctx.keg.join("cacert.pem");
        fs::write(&source, "M").unwrap();
        let args = [source.display().to_string(), "b".to_string()];
        let bundle = CaBundle::detect(&ctx, OsStr::new("post-install"), &args).unwrap();
        assert!(
            !bundle.script_is_known,
            "an unreadable script is not a known one"
        );
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args[..1]).is_none());
    }
}
