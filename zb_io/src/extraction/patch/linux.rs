use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use tracing::warn;
use zb_core::Error;

use super::text;

const LINUX_HOMEBREW_PREFIX: &str = "/home/linuxbrew/.linuxbrew";

/// Patch @@HOMEBREW_CELLAR@@ and @@HOMEBREW_PREFIX@@ placeholders in both ELF binaries and text files.
#[cfg(target_os = "linux")]
pub fn patch_placeholders(
    keg_path: &Path,
    prefix_dir: &Path,
    _pkg_name: &str,
    _pkg_version: &str,
) -> Result<(), Error> {
    patch_elf_placeholders(keg_path, prefix_dir)?;
    patch_text_placeholders(keg_path, prefix_dir)?;
    Ok(())
}

fn rewrite_homebrew_prefixes(input: &str, prefix_dir: &Path) -> String {
    let prefix_str = prefix_dir.to_string_lossy().into_owned();
    input
        .replace("@@HOMEBREW_PREFIX@@", &prefix_str)
        .replace("@@HOMEBREW_REPOSITORY@@", &prefix_str)
        .replace("@@HOMEBREW_LIBRARY@@", &format!("{}/Library", prefix_str))
        .replace(LINUX_HOMEBREW_PREFIX, &prefix_str)
}

/// Detect if zerobrew has installed its own glibc and return the path to its ld.so interpreter.
/// Returns None if zerobrew's glibc is not found, indicating we should use the system ld.so.
fn detect_zerobrew_glibc(prefix_dir: &Path) -> Option<PathBuf> {
    let cellar = prefix_dir.join("Cellar").join("glibc");

    if !cellar.exists() {
        return None;
    }

    // Look for glibc installations in the Cellar
    let glibc_entries = match fs::read_dir(&cellar) {
        Ok(entries) => entries,
        Err(_) => return None,
    };

    // Find the most recent glibc version directory
    let mut glibc_versions: Vec<PathBuf> = glibc_entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    if glibc_versions.is_empty() {
        return None;
    }

    // Sort to get the newest version (simple lexicographic sort should work for version numbers)
    glibc_versions.sort();
    glibc_versions.reverse();

    // Look for the ld.so interpreter in the glibc lib directory
    // Common names: ld-linux-x86-64.so.2, ld-linux-aarch64.so.1, ld-linux.so.2, etc.
    for glibc_dir in glibc_versions {
        let lib_dir = glibc_dir.join("lib");
        if !lib_dir.exists() {
            continue;
        }

        let entries = match fs::read_dir(&lib_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let filename = match path.file_name() {
                Some(name) => name.to_string_lossy(),
                None => continue,
            };

            // Match ld-linux*.so* patterns
            if filename.starts_with("ld-linux") && filename.contains(".so") {
                return Some(path);
            }
            // Also check for ld64.so.2 (ppc64)
            if filename == "ld64.so.2" || filename.starts_with("ld64.so.") {
                return Some(path);
            }
            // And ld-linux.so.* variants
            if filename.starts_with("ld-linux.so.") {
                return Some(path);
            }
        }
    }

    None
}

/// Find the system's dynamic linker (ld.so).
/// Returns the path to the system ld.so if found, None otherwise.
fn find_system_ld_so() -> Option<PathBuf> {
    // Common paths for system dynamic linkers on Linux
    let candidates = [
        "/lib64/ld-linux-x86-64.so.2",     // x86_64
        "/usr/lib64/ld-linux-x86-64.so.2", // x86_64
        "/lib/ld-linux-aarch64.so.1",      // aarch64/ARM64
        "/usr/lib/ld-linux-aarch64.so.1",  // aarch64/ARM64
        "/lib/ld-linux-armhf.so.3",        // ARM hard float
        "/usr/lib/ld-linux-armhf.so.3",    // ARM hard float
        "/lib/ld-linux.so.3",              // ARM
        "/lib/ld-linux.so.2",              // old ARM
        "/lib64/ld64.so.2",                // ppc64
        "/lib64/ld64.so.1",                // s390x
    ];

    for candidate in &candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Whether `path` is an ELF executable or shared library, the only kinds the
/// dynamic loader reads RPATH and the interpreter from. Relocatable objects,
/// like the `.o` files llvm ships, are left alone: there is nothing in them to
/// patch, and arwen can't parse the C++ ones.
fn is_loadable_elf(path: &Path) -> bool {
    let mut header = [0u8; 18];
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    if file.read_exact(&mut header).is_err() || header[..4] != *b"\x7fELF" {
        return false;
    }
    let e_type = match object::elf::DataEncoding(header[5]) {
        object::elf::ELFDATA2LSB => u16::from_le_bytes([header[16], header[17]]),
        object::elf::ELFDATA2MSB => u16::from_be_bytes([header[16], header[17]]),
        _ => return false,
    };
    e_type == object::elf::ET_EXEC.0 || e_type == object::elf::ET_DYN.0
}

/// Patch @@HOMEBREW_CELLAR@@ and @@HOMEBREW_PREFIX@@ placeholders in ELF binaries.
/// Uses `arwen` crate to natively update RPATH, RUNPATH, and optionally the ELF interpreter.
fn patch_elf_placeholders(keg_path: &Path, prefix_dir: &Path) -> Result<(), Error> {
    let lib_path = prefix_dir.join("lib").to_string_lossy().to_string();

    // Detect if zerobrew has installed its own glibc
    let zerobrew_interpreter = detect_zerobrew_glibc(prefix_dir);

    // Determine which interpreter to use:
    // - If zerobrew has glibc, use zerobrew's ld.so
    // - Otherwise, use the system ld.so (fallback)
    let target_interpreter = if let Some(ref zb_ld) = zerobrew_interpreter {
        Some(zb_ld.clone())
    } else {
        // Find system ld.so - common paths for Linux
        find_system_ld_so()
    };

    // Collect the ELF executables and shared libraries
    let elf_files: Vec<PathBuf> = walkdir::WalkDir::new(keg_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| is_loadable_elf(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();

    let patch_failures = AtomicUsize::new(0);
    // Use a dashmap or similar for thread-safe inode tracking if needed,
    // but we can just collect and then process, or use a Mutex.
    let processed_inodes = std::sync::Mutex::new(std::collections::HashSet::new());

    // Clone for use in parallel closure
    let target_interpreter = target_interpreter.clone();
    let new_prefix = prefix_dir.to_string_lossy().to_string();

    elf_files.par_iter().for_each(|path| {
        // Check hardlinks
        if let Ok(meta) = fs::metadata(path) {
            use std::os::unix::fs::MetadataExt;
            let inode = (meta.dev(), meta.ino());
            let mut inodes = processed_inodes.lock().unwrap();
            if !inodes.insert(inode) {
                return; // Already processed this inode
            }
        }

        // Get permissions and make writable if needed
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let original_mode = metadata.permissions().mode();
        let is_readonly = original_mode & 0o200 == 0;

        if is_readonly {
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode | 0o200);
            if let Err(e) = fs::set_permissions(path, perms) {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to make ELF writable for patching"
                );
                patch_failures.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }

        let result = (|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let content = fs::read(path)?;
            let mut elf = arwen::elf::ElfContainer::parse(&content)?;

            // Check if it is a dynamic ELF
            let has_dynamic_segment = elf
                .inner
                .builder()
                .segments
                .iter()
                .any(|s| s.p_type == object::elf::PT_DYNAMIC.0);
            if !has_dynamic_segment {
                return Ok(());
            }

            // Set page size for alignment
            let page_size = elf.get_page_size();
            let _ = elf.set_page_size(page_size);

            // RPATH
            let old_rpaths = elf.get_rpath();
            let mut new_rpaths: Vec<String> = if old_rpaths.is_empty() {
                Vec::new()
            } else {
                old_rpaths
                    .iter()
                    .map(|r| rewrite_homebrew_prefixes(r, prefix_dir))
                    .filter(|r| r.starts_with(&new_prefix) || r.starts_with("$ORIGIN"))
                    .collect()
            };

            if !new_rpaths.contains(&lib_path) {
                new_rpaths.push(lib_path.clone());
            }

            let new_rpath_str = new_rpaths.join(":");
            if !new_rpath_str.is_empty() {
                let _ = elf.set_runpath(&new_rpath_str);
            }

            // Interpreter
            let is_executable = elf.inner.builder().header.e_type == object::elf::ET_EXEC.0
                || (elf.inner.builder().header.e_type == object::elf::ET_DYN.0
                    && elf.inner.elf_interpreter().is_some());

            if is_executable && let Some(current_interp_bytes) = elf.inner.elf_interpreter() {
                let current_interp_str = String::from_utf8_lossy(current_interp_bytes);

                let target_interp_path = if current_interp_str.contains("@@HOMEBREW_PREFIX@@")
                    || current_interp_str.contains(LINUX_HOMEBREW_PREFIX)
                {
                    let expanded = rewrite_homebrew_prefixes(&current_interp_str, prefix_dir);
                    let expanded_path = PathBuf::from(&expanded);
                    if expanded_path.exists() {
                        Some(expanded_path)
                    } else {
                        find_system_ld_so()
                    }
                } else {
                    target_interpreter.clone()
                };

                if let Some(target_path) = target_interp_path {
                    let target_str = target_path.to_string_lossy();
                    let _ = elf.set_interpreter(&target_str);
                }
            }

            // Atomic write
            let temp_path = path.with_extension("tmp_patch");
            {
                let mut temp_file = fs::File::create(&temp_path)?;
                elf.write(&mut temp_file)?;
            }
            fs::rename(temp_path, path)?;

            // Restore original permissions (including execute bit) after atomic write
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode);
            fs::set_permissions(path, perms)?;

            Ok(())
        })();

        if let Err(e) = result {
            warn!(path = %path.display(), error = %e, "failed to patch ELF");
            patch_failures.fetch_add(1, Ordering::Relaxed);
        }
    });

    let failures = patch_failures.load(Ordering::Relaxed);
    if failures > 0 {
        warn!(
            failures,
            "failed to patch ELF files; packages may not work correctly until manually patched"
        );
    }

    Ok(())
}

/// Patch text files containing @@HOMEBREW_...@@ placeholders
fn patch_text_placeholders(keg_path: &Path, prefix_dir: &Path) -> Result<(), Error> {
    let prefix_str = prefix_dir.to_string_lossy().into_owned();
    let cellar_str = prefix_dir.join("Cellar").to_string_lossy().into_owned();
    let library_str = format!("{prefix_str}/Library");
    let replacements = [
        ("@@HOMEBREW_PREFIX@@", prefix_str.as_str()),
        ("@@HOMEBREW_REPOSITORY@@", prefix_str.as_str()),
        ("@@HOMEBREW_LIBRARY@@", library_str.as_str()),
        (LINUX_HOMEBREW_PREFIX, prefix_str.as_str()),
        ("@@HOMEBREW_CELLAR@@", cellar_str.as_str()),
        ("@@HOMEBREW_PERL@@", "/usr/bin/perl"),
        ("@@HOMEBREW_JAVA@@", "/usr/bin/java"),
    ];

    let files: Vec<PathBuf> = walkdir::WalkDir::new(keg_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();

    files.par_iter().for_each(|path| {
        if let Err(e) = text::rewrite_text_file(path, &replacements) {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to patch text file"
            );
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use std::process::Command;
    use tempfile::TempDir;

    fn compile_dummy_elf(dir: &Path, name: &str) -> Option<PathBuf> {
        let src_path = dir.join(format!("{}.c", name));
        if fs::write(&src_path, "int main() { return 0; }").is_err() {
            return None;
        }

        let out_path = dir.join(name);
        let status = Command::new("cc")
            .arg(&src_path)
            .arg("-o")
            .arg(&out_path)
            .arg("-Wl,-rpath,@@HOMEBREW_PREFIX@@/lib")
            .status()
            .ok()?;

        if status.success() {
            Some(out_path)
        } else {
            None
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn rewrites_linuxbrew_prefixes() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");

        assert_eq!(
            rewrite_homebrew_prefixes(
                "/home/linuxbrew/.linuxbrew/opt/expat/lib:@@HOMEBREW_PREFIX@@/lib",
                &prefix
            ),
            format!(
                "{}/opt/expat/lib:{}/lib",
                prefix.display(),
                prefix.display()
            )
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn patches_text_files() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        let pkg_dir = cellar.join("testpkg/1.0.0");
        let bin_dir = pkg_dir.join("bin");

        fs::create_dir_all(&bin_dir).unwrap();

        let script_path = bin_dir.join("script.sh");
        fs::write(
            &script_path,
            "#!/home/linuxbrew/.linuxbrew/opt/python@3.14/bin/python3.14\necho @@HOMEBREW_PREFIX@@\necho @@HOMEBREW_CELLAR@@\necho @@HOMEBREW_LIBRARY@@\necho @@HOMEBREW_PERL@@",
        )
        .unwrap();

        let result = patch_placeholders(&pkg_dir, &prefix, "testpkg", "1.0.0");
        assert!(result.is_ok());

        let content = fs::read_to_string(&script_path).unwrap();
        assert!(content.contains(prefix.to_str().unwrap()));
        assert!(content.starts_with(&format!(
            "#!{}/opt/python@3.14/bin/python3.14",
            prefix.display()
        )));
        assert!(!content.contains(LINUX_HOMEBREW_PREFIX));
        assert!(content.contains(cellar.to_str().unwrap()));
        assert!(content.contains(&format!("{}/Library", prefix.to_str().unwrap())));
        assert!(content.contains("/usr/bin/perl"));
        assert!(!content.contains("@@HOMEBREW_"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn patches_elf_file() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        let pkg_dir = cellar.join("testpkg/1.0.0");
        let bin_dir = pkg_dir.join("bin");

        fs::create_dir_all(&bin_dir).unwrap();

        let elf_path = match compile_dummy_elf(&bin_dir, "testbin") {
            Some(p) => p,
            None => {
                eprintln!("Skipping ELF patch test: cc not found");
                return;
            }
        };

        // Record original permissions (should include execute bit from cc)
        let original_mode = fs::metadata(&elf_path).unwrap().permissions().mode();
        assert!(
            original_mode & 0o111 != 0,
            "compiled binary should be executable"
        );

        let result = patch_placeholders(&pkg_dir, &prefix, "testpkg", "1.0.0");
        assert!(result.is_ok());

        // Verify permissions are preserved after patching
        let new_mode = fs::metadata(&elf_path).unwrap().permissions().mode();
        assert_eq!(
            original_mode & 0o777,
            new_mode & 0o777,
            "permissions should be preserved after patching"
        );
    }

    fn elf_header(data: u8, e_type: u16) -> Vec<u8> {
        let mut header = b"\x7fELF".to_vec();
        header.extend([2, data, 1]);
        header.resize(16, 0);
        match data {
            2 => header.extend(e_type.to_be_bytes()),
            _ => header.extend(e_type.to_le_bytes()),
        }
        header.resize(64, 0);
        header
    }

    #[test]
    fn loadable_elf_is_executables_and_shared_libraries() {
        let tmp = TempDir::new().unwrap();
        let cases: [(&str, Vec<u8>, bool); 8] = [
            ("exec", elf_header(1, 2), true),
            ("dyn", elf_header(1, 3), true),
            ("dyn-big-endian", elf_header(2, 3), true),
            ("rel", elf_header(1, 1), false),
            ("rel-big-endian", elf_header(2, 1), false),
            ("core", elf_header(1, 4), false),
            ("unknown-encoding", elf_header(0, 3), false),
            ("truncated", b"\x7fELF\x02\x01".to_vec(), false),
        ];
        for (name, bytes, expected) in cases {
            let path = tmp.path().join(name);
            fs::write(&path, bytes).unwrap();
            assert_eq!(is_loadable_elf(&path), expected, "{name}");
        }
        let script = tmp.path().join("script");
        fs::write(&script, "#!/bin/sh\necho hello world\n").unwrap();
        assert!(!is_loadable_elf(&script));
        assert!(!is_loadable_elf(&tmp.path().join("missing")));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn skips_cpp_objects_arwen_cannot_parse() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("object.cpp");
        // A template and an inline function give the object COMDAT groups,
        // which arwen rejects as an unsupported section type.
        fs::write(
            &src,
            "template<class T> T id(T x) { return x; }\ninline int g() { static int s = 1; return s; }\nint use() { return id(1) + g(); }\n",
        )
        .unwrap();
        let object = tmp.path().join("object.o");
        let Ok(status) = Command::new("c++")
            .arg("-c")
            .arg(&src)
            .arg("-o")
            .arg(&object)
            .status()
        else {
            eprintln!("Skipping C++ object test: c++ not found");
            return;
        };
        assert!(status.success());

        assert!(!is_loadable_elf(&object));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_glibc_detection() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");

        // Test 1: No glibc installed - should return None
        assert!(detect_zerobrew_glibc(&prefix).is_none());

        // Test 2: Create a mock glibc installation
        let glibc_dir = prefix.join("Cellar/glibc/2.38");
        let lib_dir = glibc_dir.join("lib");
        fs::create_dir_all(&lib_dir).unwrap();

        // Create a mock ld-linux-x86-64.so.2
        let ld_so = lib_dir.join("ld-linux-x86-64.so.2");
        fs::write(&ld_so, "mock").unwrap();

        // Should now detect the glibc
        let detected = detect_zerobrew_glibc(&prefix);
        assert!(detected.is_some());
        assert_eq!(detected.unwrap(), ld_so);

        // Test 3: Multiple glibc versions - should pick the newest
        let glibc_dir_newer = prefix.join("Cellar/glibc/2.39");
        let lib_dir_newer = glibc_dir_newer.join("lib");
        fs::create_dir_all(&lib_dir_newer).unwrap();
        let ld_so_newer = lib_dir_newer.join("ld-linux-x86-64.so.2");
        fs::write(&ld_so_newer, "mock").unwrap();

        let detected = detect_zerobrew_glibc(&prefix);
        assert!(detected.is_some());
        assert_eq!(detected.unwrap(), ld_so_newer);
    }
}
