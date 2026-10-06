use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::warn;
use zb_core::Error;

use super::text;

/// Whether `path` is a Mach-O file, judging by its magic number.
pub(crate) fn is_macho(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && matches!(
            u32::from_be_bytes(magic),
            0xfeedface | 0xfeedfacf | 0xcafebabe | 0xcefaedfe | 0xcffaedfe
        )
}

/// Ad-hoc sign a binary whose signature was invalidated by patching, keeping
/// its entitlements, requirements, flags and hardened runtime. If codesign
/// fails, retry once on a fresh copy of the file, which is how Homebrew works
/// around a codesign bug.
fn resign(path: &Path) -> Result<(), String> {
    let sign = || {
        Command::new("codesign")
            .args([
                "--sign",
                "-",
                "--force",
                "--preserve-metadata=entitlements,requirements,flags,runtime",
            ])
            .arg(path)
            .output()
    };
    if matches!(sign(), Ok(output) if output.status.success()) {
        return Ok(());
    }

    let mut copy = path.as_os_str().to_owned();
    copy.push(".zb-resign");
    fs::copy(path, &copy).map_err(|e| e.to_string())?;
    fs::rename(&copy, path).map_err(|e| e.to_string())?;

    match sign() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Patch Homebrew placeholders, and the prefix the bottle was built in, in a
/// text file.
fn patch_text_file_strings(
    path: &Path,
    build_prefix: &str,
    new_prefix: &str,
    new_cellar: &str,
) -> Result<(), Error> {
    let new_library = format!("{new_prefix}/Library");
    let replacements = [
        ("@@HOMEBREW_PREFIX@@", new_prefix),
        ("@@HOMEBREW_CELLAR@@", new_cellar),
        ("@@HOMEBREW_REPOSITORY@@", new_prefix),
        ("@@HOMEBREW_LIBRARY@@", new_library.as_str()),
        ("@@HOMEBREW_PERL@@", "/usr/bin/perl"),
        ("@@HOMEBREW_JAVA@@", "/usr/bin/java"),
        (build_prefix, new_prefix),
    ];
    text::rewrite_text_file(path, &replacements)
        .map_err(Error::store("failed to patch text file"))?;
    Ok(())
}

/// Patch hardcoded Homebrew paths in a binary file's strings, in place.
/// This handles paths like /opt/homebrew/opt/git/libexec/git-core that are
/// baked into Mach-O binaries and static archives.
/// Only `build_prefix`, the prefix the bottle was built in, is rewritten: other
/// paths like /usr/local on Apple Silicon aren't Homebrew's and must be left alone.
///
/// A shorter prefix is padded with `/` to the old length, as Homebrew does, so
/// the rest of the path is still reachable: `/opt/homebrew/Cellar/x` becomes
/// `/tmp/zb/////Cellar/x`, not a string that ends at the prefix.
fn patch_binary_strings(path: &Path, build_prefix: &str, new_prefix: &str) -> Result<(), Error> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    if build_prefix == new_prefix {
        return Ok(());
    }

    let metadata = fs::metadata(path).map_err(Error::store("failed to read metadata"))?;
    let original_mode = metadata.permissions().mode();
    let is_readonly = original_mode & 0o200 == 0;

    let mut contents = fs::read(path).map_err(Error::store("failed to read file"))?;
    let old_bytes = build_prefix.as_bytes();
    let new_bytes = new_prefix.as_bytes();

    if new_bytes.len() > old_bytes.len() {
        // A longer path can't be written in place. The planner refuses to pour
        // bottles pinned to a shorter prefix, so this only happens for
        // relocatable bottles with stray references to the build prefix.
        //
        // See: https://github.com/zerobrewhq/zerobrew/issues/286
        let has_old_paths = contents
            .windows(old_bytes.len() + 1)
            .any(|w| w[..old_bytes.len()] == *old_bytes && w[old_bytes.len()] == b'/');
        if has_old_paths {
            warn!(
                path = %path.display(),
                old_prefix = %build_prefix,
                new_prefix = %new_prefix,
                "binary contains hardcoded paths under {build_prefix} that \
                could not be rewritten to {new_prefix} (new path is longer). \
                this package may not work correctly
                tracking issue: https://github.com/zerobrewhq/zerobrew/issues/286
                ",
            );
        }
        return Ok(());
    }

    let mut patched = false;
    let mut i = 0;
    while i + old_bytes.len() <= contents.len() {
        if contents[i..i + old_bytes.len()] == *old_bytes
            && matches!(
                contents.get(i + old_bytes.len()).copied(),
                None | Some(0) | Some(b'/') | Some(b':')
            )
        {
            contents[i..i + new_bytes.len()].copy_from_slice(new_bytes);
            contents[i + new_bytes.len()..i + old_bytes.len()].fill(b'/');
            patched = true;
            i += old_bytes.len();
        } else {
            i += 1;
        }
    }

    if !patched {
        return Ok(());
    }

    if is_readonly {
        let mut perms = metadata.permissions();
        perms.set_mode(original_mode | 0o200);
        fs::set_permissions(path, perms).map_err(Error::store("failed to make writable"))?;
    }

    let temp_path = path.with_extension("tmp_patch");
    let mut temp_file =
        fs::File::create(&temp_path).map_err(Error::store("failed to create temp file"))?;
    temp_file
        .write_all(&contents)
        .map_err(Error::store("failed to write temp file"))?;
    drop(temp_file);

    fs::rename(&temp_path, path).map_err(Error::store("failed to rename temp file"))?;

    // fs::File::create uses 0644 by default, which drops the execute bit from
    // patched binaries. Keep the file writable until it's re-signed.
    let mut perms = metadata.permissions();
    perms.set_mode(original_mode | 0o200);
    fs::set_permissions(path, perms)
        .map_err(Error::store("failed to restore permissions after patching"))?;

    // Only Mach-O files carry a signature; a static archive has nothing to re-sign.
    if is_macho(path)
        && let Err(e) = resign(path)
    {
        warn!(
            path = %path.display(),
            error = %e,
            "failed to re-sign patched file; it may be killed when run"
        );
    }

    fs::set_permissions(path, metadata.permissions())
        .map_err(Error::store("failed to restore permissions after patching"))?;

    Ok(())
}

/// Patch @@HOMEBREW_CELLAR@@ and @@HOMEBREW_PREFIX@@ placeholders in Mach-O binaries.
/// Also fixes version mismatches where a bottle references a different version of itself.
/// Additionally patches hardcoded paths under `build_prefix`, the prefix the
/// bottle was built in, in binary data sections and text files.
/// Uses rayon for parallel processing.
pub fn patch_homebrew_placeholders(
    keg_path: &Path,
    cellar_dir: &Path,
    pkg_name: &str,
    pkg_version: &str,
    build_prefix: &str,
) -> Result<(), Error> {
    use rayon::prelude::*;
    use regex::Regex;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // Derive prefix from cellar (cellar_dir is typically prefix/Cellar)
    let prefix = cellar_dir.parent().unwrap_or(Path::new("/opt/homebrew"));

    let cellar_str = cellar_dir.to_string_lossy().to_string();
    let prefix_str = prefix.to_string_lossy().to_string();

    let version_pattern = format!(r"(/Cellar/{}/)([^/]+)(/)", regex::escape(pkg_name));
    let version_regex = Regex::new(&version_pattern).ok();

    // Collect all regular files first (skip symlinks to avoid double-processing)
    let files: Vec<PathBuf> = walkdir::WalkDir::new(keg_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();

    // Binary files need their strings rewritten in place. That is every file
    // with a NUL byte, not just Mach-O: static archives embed the build
    // prefix too (pkgconf's libpkgconf.a carries its personality.d path).
    let binary_files: Vec<&PathBuf> = files
        .iter()
        .filter(|path| text::is_binary(path).unwrap_or(false))
        .collect();
    let macho_files: Vec<&PathBuf> = binary_files
        .iter()
        .copied()
        .filter(|path| is_macho(path))
        .collect();

    let patch_failures = AtomicUsize::new(0);
    let first_patch_error: Arc<Mutex<Option<Error>>> = Arc::new(Mutex::new(None));

    // First pass: patch strings in binary files
    binary_files.par_iter().for_each(|path| {
        if let Err(e) = patch_binary_strings(path, build_prefix, &prefix_str) {
            patch_failures.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut guard) = first_patch_error.lock()
                && guard.is_none()
            {
                *guard = Some(e);
            }
        }
    });

    if let Ok(mut guard) = first_patch_error.lock()
        && let Some(e) = guard.take()
    {
        return Err(e);
    }

    // Second pass: patch text files (the patcher skips binaries itself)
    files.par_iter().for_each(|path| {
        if let Err(e) = patch_text_file_strings(path, build_prefix, &prefix_str, &cellar_str) {
            warn!(path = %path.display(), error = %e, "failed to patch text file");
        }
    });

    // Helper to patch a single path reference
    let patch_path = |old_path: &str| -> Option<String> {
        let mut new_path = old_path.to_string();
        let mut changed = false;

        // Replace Homebrew placeholders
        if old_path.contains("@@HOMEBREW_CELLAR@@") || old_path.contains("@@HOMEBREW_PREFIX@@") {
            new_path = new_path
                .replace("@@HOMEBREW_CELLAR@@", &cellar_str)
                .replace("@@HOMEBREW_PREFIX@@", &prefix_str);
            changed = true;
        }

        // Fix version mismatches for this package
        if let Some(re) = &version_regex
            && re.is_match(&new_path)
        {
            let replacement = format!("/Cellar/{}/{}/", pkg_name, pkg_version);
            let fixed = re.replace(&new_path, |caps: &regex::Captures| {
                let matched_version = &caps[2];
                if matched_version != pkg_version {
                    replacement.clone()
                } else {
                    caps[0].to_string()
                }
            });
            if fixed != new_path {
                new_path = fixed.to_string();
                changed = true;
            }
        }

        if changed && new_path != old_path {
            Some(new_path)
        } else {
            None
        }
    };

    // install_name_tool exits non-zero when a new path doesn't fit in the
    // binary's header; record why so the failure isn't silent.
    let first_relocation_error: Mutex<Option<String>> = Mutex::new(None);
    let record_failure = |path: &Path, output: std::io::Result<std::process::Output>| {
        let reason = match output {
            Ok(output) if output.status.success() => return true,
            Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_string(),
            Err(e) => e.to_string(),
        };
        patch_failures.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut first) = first_relocation_error.lock() {
            first.get_or_insert_with(|| format!("{}: {reason}", path.display()));
        }
        false
    };

    // Third pass: Process Mach-O files for install_name_tool patching
    macho_files.par_iter().for_each(|path| {
        // Get file permissions and make writable if needed
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let original_mode = metadata.permissions().mode();
        let is_readonly = original_mode & 0o200 == 0;

        // Make writable for patching
        if is_readonly {
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode | 0o200);
            if fs::set_permissions(path, perms).is_err() {
                patch_failures.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }

        let mut patched_any = false;

        // Get and patch library dependencies (-L)
        if let Ok(output) = Command::new("otool")
            .args(["-L", &path.to_string_lossy()])
            .output()
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let line = line.trim();
                if let Some(old_path) = line.split_whitespace().next()
                    && let Some(new_path) = patch_path(old_path)
                {
                    let result = Command::new("install_name_tool")
                        .args(["-change", old_path, &new_path, &path.to_string_lossy()])
                        .output();
                    if record_failure(path, result) {
                        patched_any = true;
                    }
                }
            }
        }

        // Get and patch install name ID (-D)
        if let Ok(output) = Command::new("otool")
            .args(["-D", &path.to_string_lossy()])
            .output()
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines().skip(1) {
                // Skip first line (filename)
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(new_id) = patch_path(line) {
                    let result = Command::new("install_name_tool")
                        .args(["-id", &new_id, &path.to_string_lossy()])
                        .output();
                    if record_failure(path, result) {
                        patched_any = true;
                    }
                }
            }
        }

        // Re-sign if we patched anything (patching invalidates code signature)
        if patched_any && let Err(e) = resign(path) {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to re-sign patched file; it may be killed when run"
            );
        }

        // Restore original permissions
        if is_readonly {
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode);
            let _ = fs::set_permissions(path, perms);
        }
    });

    let failures = patch_failures.load(Ordering::Relaxed);
    if failures > 0 {
        let first = first_relocation_error
            .lock()
            .ok()
            .and_then(|first| first.clone())
            .map(|reason| format!(" (first failure: {reason})"))
            .unwrap_or_default();
        return Err(Error::StoreCorruption {
            message: format!(
                "failed to patch {} Mach-O files in {}{first}",
                failures,
                keg_path.display()
            ),
        });
    }

    Ok(())
}

/// Strip quarantine extended attributes and ad-hoc sign unsigned Mach-O binaries.
/// Homebrew bottles from ghcr.io are already adhoc signed, so this is mostly a no-op.
/// We use a fast heuristic: only process binaries that fail signature verification.
pub fn codesign_and_strip_xattrs(keg_path: &Path) -> Result<(), Error> {
    use rayon::prelude::*;
    use std::os::unix::fs::PermissionsExt;

    // First, do a quick recursive xattr strip (single command, very fast)
    let _ = Command::new("xattr")
        .args(["-rd", "com.apple.quarantine", &keg_path.to_string_lossy()])
        .stderr(std::process::Stdio::null())
        .output();
    let _ = Command::new("xattr")
        .args(["-rd", "com.apple.provenance", &keg_path.to_string_lossy()])
        .stderr(std::process::Stdio::null())
        .output();

    // Find executables in bin/ directories only (where signing matters)
    // Skip dylibs and other Mach-O files - they inherit signing from their loader
    let bin_files: Vec<PathBuf> = walkdir::WalkDir::new(keg_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            path.is_file() && path.to_string_lossy().contains("/bin/")
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    // Only process files that need signing
    bin_files.par_iter().for_each(|path| {
        if !is_macho(path) {
            return;
        }

        // Verify signature - if valid, skip
        let verify = Command::new("codesign")
            .args(["-v", &path.to_string_lossy()])
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();

        if verify.map(|s| s.success()).unwrap_or(false) {
            return; // Already signed
        }

        // Get permissions and make writable
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let original_mode = metadata.permissions().mode();
        let is_readonly = original_mode & 0o200 == 0;

        if is_readonly {
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode | 0o200);
            let _ = fs::set_permissions(path, perms);
        }

        // Sign the binary
        let _ = Command::new("codesign")
            .args(["--force", "--sign", "-", &path.to_string_lossy()])
            .output();

        // Restore permissions
        if is_readonly {
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode);
            let _ = fs::set_permissions(path, perms);
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn test_patch_binary_preserves_execute_bit() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("test_binary");

        let old_prefix = "/home/linuxbrew/.linuxbrew";
        let new_prefix = "/opt/zerobrew/prefix";

        let mut contents = Vec::new();
        contents.extend_from_slice(b"\xfe\xed\xfa\xcf");
        contents.extend_from_slice(old_prefix.as_bytes());
        contents.extend_from_slice(b"/bin/hello\0");

        fs::write(&test_file, &contents).unwrap();

        // Set executable permissions (0755)
        let mut perms = fs::metadata(&test_file).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&test_file, perms).unwrap();

        patch_binary_strings(&test_file, old_prefix, new_prefix).unwrap();

        let mode = fs::metadata(&test_file).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "execute bit lost after patching: mode = {:#o}",
            mode
        );
    }

    #[test]
    fn test_patch_binary_strings() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("test_binary");

        let old_prefix = "/home/linuxbrew/.linuxbrew";
        let new_prefix = "/opt/zerobrew/prefix";

        let mut contents = Vec::new();
        contents.extend_from_slice(b"\xfe\xed\xfa\xcf");
        contents.extend_from_slice(b"some random data\0");
        contents.extend_from_slice(old_prefix.as_bytes());
        contents.extend_from_slice(b"/opt/git/libexec/git-core\0");
        contents.extend_from_slice(b"more data\0");
        contents.extend_from_slice(old_prefix.as_bytes());
        contents.extend_from_slice(b"/lib/libfoo.dylib\0");
        contents.extend_from_slice(b"end\0");

        fs::write(&test_file, &contents).unwrap();

        let result = patch_binary_strings(&test_file, old_prefix, new_prefix);
        assert!(result.is_ok());

        let patched = fs::read(&test_file).unwrap();
        let patched_str = String::from_utf8_lossy(&patched);

        assert!(patched_str.contains(new_prefix));
        assert!(!patched_str.contains(old_prefix));
    }

    #[test]
    fn test_patch_binary_pads_a_shorter_prefix_with_slashes() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("test_binary");

        let mut contents = Vec::new();
        contents.extend_from_slice(b"\xfe\xed\xfa\xcf");
        contents.extend_from_slice(b"/opt/homebrew/Cellar/pkgconf/3.0.7/share\0");
        contents.extend_from_slice(b"/opt/homebrew/lib:/opt/homebrew/share\0");
        contents.extend_from_slice(b"/opt/homebrew\0");
        let original_len = contents.len();
        fs::write(&test_file, &contents).unwrap();

        patch_binary_strings(&test_file, "/opt/homebrew", "/tmp/zb").unwrap();

        let patched = fs::read(&test_file).unwrap();
        assert_eq!(
            patched.len(),
            original_len,
            "rewrite must keep the file size"
        );
        let patched = String::from_utf8_lossy(&patched);
        // The tail of each path must still be reachable: NUL padding would end
        // the C string at the prefix.
        assert!(patched.contains("/tmp/zb///////Cellar/pkgconf/3.0.7/share\0"));
        assert!(patched.contains("/tmp/zb///////lib:/tmp/zb///////share\0"));
        assert!(patched.contains("/tmp/zb//////\0"));
        assert!(!patched.contains("/opt/homebrew"));
    }

    #[test]
    fn test_patch_binary_rewrites_static_archives() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("libfoo.a");

        // An `ar` archive is binary but not Mach-O; Homebrew rewrites it too.
        let mut contents = Vec::new();
        contents.extend_from_slice(b"!<arch>\n");
        contents.extend_from_slice(b"foo.o/          0  0  0  644  64  `\n");
        contents.extend_from_slice(b"\xcf\xfa\xed\xfe\0\0\0\0");
        contents.extend_from_slice(b"/opt/homebrew/share/pkgconfig/personality.d\0");
        fs::write(&test_file, &contents).unwrap();

        patch_binary_strings(&test_file, "/opt/homebrew", "/opt/zerobrew").unwrap();

        let patched = String::from_utf8_lossy(&fs::read(&test_file).unwrap()).into_owned();
        assert!(patched.contains("/opt/zerobrew/share/pkgconfig/personality.d"));
        assert!(!patched.contains("/opt/homebrew"));
    }

    #[test]
    fn test_patch_binary_skips_when_new_prefix_longer() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("test_binary");

        let old_prefix = "/opt/homebrew";
        let new_prefix = "/opt/zerobrew/prefix";

        let mut contents = Vec::new();
        contents.extend_from_slice(b"\xfe\xed\xfa\xcf");
        contents.extend_from_slice(b"some random data\0");
        contents.extend_from_slice(old_prefix.as_bytes());
        contents.extend_from_slice(b"/opt/git/libexec/git-core\0");
        contents.extend_from_slice(b"more data\0");

        let original = contents.clone();
        fs::write(&test_file, &contents).unwrap();

        // Should succeed (skip) rather than error when the new prefix is
        // longer than the old one — install_name_tool handles load command
        // changes regardless of length.
        let result = patch_binary_strings(&test_file, old_prefix, new_prefix);
        assert!(
            result.is_ok(),
            "should skip when new prefix is longer than old prefix"
        );

        let unchanged = fs::read(&test_file).unwrap();
        assert_eq!(
            unchanged, original,
            "binary must be unchanged when prefix cannot be expanded in-place"
        );
    }

    #[test]
    fn test_version_regex_only_matches_cellar_paths() {
        use regex::Regex;

        let pkg_name = "mpdecimal";
        let pkg_version = "4.0.1";
        let pattern = format!(r"(/Cellar/{}/)([^/]+)(/)", regex::escape(pkg_name));
        let re = Regex::new(&pattern).expect("version regex should compile");

        let cellar_path = "/opt/zerobrew/Cellar/mpdecimal/3.9.0/lib/libmpdec.4.dylib";
        assert!(re.is_match(cellar_path));

        let replacement = format!("/Cellar/{}/{}/", pkg_name, pkg_version);
        let fixed = re.replace(cellar_path, |caps: &regex::Captures| {
            let matched_version = &caps[2];
            if matched_version != pkg_version {
                replacement.clone()
            } else {
                caps[0].to_string()
            }
        });
        assert_eq!(
            fixed,
            "/opt/zerobrew/Cellar/mpdecimal/4.0.1/lib/libmpdec.4.dylib"
        );

        let opt_path = "/opt/zerobrew/opt/mpdecimal/lib/libmpdec.4.dylib";
        assert!(!re.is_match(opt_path));

        let cellar_same_version = "/opt/zerobrew/Cellar/mpdecimal/4.0.1/lib/libmpdec.4.dylib";
        let unchanged = re.replace(cellar_same_version, |caps: &regex::Captures| {
            let matched_version = &caps[2];
            if matched_version != pkg_version {
                replacement.clone()
            } else {
                caps[0].to_string()
            }
        });
        assert_eq!(unchanged, cellar_same_version);
    }

    #[test]
    fn test_patch_text_file_strings() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("test_script.sh");

        let content = r#"#!/bin/bash
export GIT_EXEC_PATH=/opt/homebrew/opt/git/libexec/git-core
export PREFIX=@@HOMEBREW_PREFIX@@
export CELLAR=@@HOMEBREW_CELLAR@@
export LIBRARY=@@HOMEBREW_LIBRARY@@
export PERL=@@HOMEBREW_PERL@@
echo "Hello from $PREFIX"
"#;

        fs::write(&test_file, content).unwrap();

        let new_prefix = "/opt/zerobrew/prefix";
        let new_cellar = format!("{}/Cellar", new_prefix);

        let result = patch_text_file_strings(&test_file, "/opt/homebrew", new_prefix, &new_cellar);
        assert!(result.is_ok());

        let patched = fs::read_to_string(&test_file).unwrap();
        assert!(patched.contains(new_prefix));
        assert!(!patched.contains("/opt/homebrew"));
        assert!(!patched.contains("@@HOMEBREW_"));
        assert!(patched.contains("/opt/zerobrew/prefix/opt/git/libexec/git-core"));
        assert!(patched.contains("/opt/zerobrew/prefix/Cellar"));
        assert!(patched.contains("/opt/zerobrew/prefix/Library"));
        assert!(patched.contains("/usr/bin/perl"));
    }

    #[test]
    fn test_patch_binary_only_rewrites_the_build_prefix() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("test_binary");

        let mut contents = Vec::new();
        contents.extend_from_slice(b"\xfe\xed\xfa\xcf");
        contents.extend_from_slice(b"/opt/homebrew/etc/gitconfig\0");
        contents.extend_from_slice(b"/usr/local/lib/node_modules\0");
        fs::write(&test_file, &contents).unwrap();

        patch_binary_strings(&test_file, "/opt/homebrew", "/opt/zerobrew").unwrap();

        let patched = String::from_utf8_lossy(&fs::read(&test_file).unwrap()).into_owned();
        assert!(patched.contains("/opt/zerobrew/etc/gitconfig"));
        assert!(!patched.contains("/opt/homebrew"));
        assert!(
            patched.contains("/usr/local/lib/node_modules"),
            "paths outside the build prefix must be left alone"
        );
    }

    #[test]
    fn test_patch_text_file_leaves_other_prefixes_alone() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("script.sh");
        fs::write(
            &test_file,
            "#!/bin/sh\nPATH=/usr/local/bin:@@HOMEBREW_PREFIX@@/bin\n",
        )
        .unwrap();

        patch_text_file_strings(
            &test_file,
            "/opt/homebrew",
            "/opt/zerobrew",
            "/opt/zerobrew/Cellar",
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(&test_file).unwrap(),
            "#!/bin/sh\nPATH=/usr/local/bin:/opt/zerobrew/bin\n"
        );
    }
}
