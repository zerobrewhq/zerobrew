use std::path::{Path, PathBuf};

/// Basenames of glibc dynamic linkers under `$TERMUX__PREFIX/glibc/lib`.
const LD_LINUX_AARCH64: &str = "ld-linux-aarch64.so.1";
const LD_LINUX_X86_64: &str = "ld-linux-x86-64.so.2";

/// Termux install root (`$TERMUX__PREFIX`), when running on Android/Termux.
pub fn termux_prefix() -> Option<PathBuf> {
    std::env::var("TERMUX__PREFIX")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Candidate glibc loader paths, highest priority first.
pub fn glibc_dynamic_linker_candidates(termux_prefix: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(from_env) = std::env::var("ZEROBREW_GLIBC_LD")
        && !from_env.is_empty()
    {
        candidates.push(PathBuf::from(from_env));
    }

    let glibc_lib = termux_prefix.join("glibc/lib");
    match std::env::consts::ARCH {
        "aarch64" => candidates.push(glibc_lib.join(LD_LINUX_AARCH64)),
        "x86_64" => candidates.push(glibc_lib.join(LD_LINUX_X86_64)),
        _ => {}
    }

    candidates
}

/// First existing glibc dynamic linker under Termux's glibc tree.
pub fn resolve_glibc_dynamic_linker(termux_prefix: &Path) -> Option<PathBuf> {
    glibc_dynamic_linker_candidates(termux_prefix)
        .into_iter()
        .find(|path| path.is_file())
}

/// Whether Termux's glibc sidecar is present enough to pour Linux bottles.
pub fn termux_glibc_ready_message() -> Result<PathBuf, String> {
    let prefix = termux_prefix().ok_or_else(|| {
        "TERMUX__PREFIX is not set; Linux bottles need the Termux glibc package".to_string()
    })?;
    resolve_glibc_dynamic_linker(&prefix).ok_or_else(|| {
        format!(
            "Termux glibc dynamic linker not found under {}/glibc/lib \
             (enable the glibc apt repo and run: pkg install glibc)",
            prefix.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn candidates_include_arch_ld_linux_under_glibc_lib() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path();
        let candidates = glibc_dynamic_linker_candidates(prefix);
        assert!(!candidates.is_empty());
        let last = candidates.last().unwrap();
        #[cfg(target_arch = "aarch64")]
        assert!(last.ends_with(LD_LINUX_AARCH64));
        #[cfg(target_arch = "x86_64")]
        assert!(last.ends_with(LD_LINUX_X86_64));
    }

    #[test]
    fn resolve_picks_first_existing_candidate() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path();
        let glibc_lib = prefix.join("glibc/lib");
        fs::create_dir_all(&glibc_lib).unwrap();
        let ld = glibc_lib.join(LD_LINUX_AARCH64);
        if std::env::consts::ARCH == "aarch64" {
            fs::write(&ld, b"").unwrap();
            assert_eq!(
                resolve_glibc_dynamic_linker(prefix).as_deref(),
                Some(ld.as_path())
            );
        }
    }

    #[test]
    fn ready_message_err_when_no_ld_so() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path();
        fs::create_dir_all(prefix.join("glibc/lib")).unwrap();
        assert!(resolve_glibc_dynamic_linker(prefix).is_none());
    }
}
