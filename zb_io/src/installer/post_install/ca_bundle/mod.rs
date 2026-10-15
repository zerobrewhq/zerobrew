//! A cache for the CA bundle ca-certificates builds in post-install.
//!
//! The formula's step runs `libexec/post-install SOURCE DESTINATION`, a
//! shell script that exports the system keychain, checks every certificate
//! with `openssl` and `security` one process at a time and merges in the
//! Mozilla bundle. That takes about five seconds, and its output depends
//! only on the Mozilla bundle it is given and the system trust store. The
//! result is kept keyed on both, so reinstalling ca-certificates, or
//! anything that depends on it, copies the bundle instead of rebuilding
//! it. The bytes are the script's own: a miss runs it unchanged.

// Not wired up yet: the in-process build that uses these comes last.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
mod macos;
#[allow(dead_code)]
mod pem;
#[allow(dead_code)]
mod purpose;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use super::steps::write_atomically;
use super::{PostInstallContext, StepError};

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
    destination: PathBuf,
    cached: PathBuf,
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

    /// Remember the bundle the script just wrote.
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
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::super::steps::run_step;
    use super::super::test_support::context;
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;
    use zb_core::{PathBase, PathSpec, StepAction, Sudo};

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

    #[test]
    fn the_bundle_is_built_once_and_restored_after() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        install_script(&ctx);
        let source = ctx.keg.join("share/ca-certificates/cacert.pem");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "MOZILLA\n").unwrap();
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
    fn the_script_is_part_of_the_cache_key() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        let script = install_script(&ctx);
        let source = ctx.keg.join("share/ca-certificates/cacert.pem");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "MOZILLA\n").unwrap();
        let args = [source.display().to_string(), "b".to_string()];
        let first = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
        fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        let second = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
        assert_ne!(first.cached, second.cached);
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
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args).is_some());
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args[..1]).is_none());
    }
}
