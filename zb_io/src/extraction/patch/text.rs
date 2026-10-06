//! Rewrite Homebrew placeholders and prefixes in a keg's text files.

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::phar;

/// Replace each `(from, to)` pair in the file at `path`, in order.
///
/// Scripts that start with `#!` are rewritten even if they contain binary
/// data, as Homebrew does: PHP archives are scripts with a binary payload.
/// Other files with NUL bytes in their first 8 KiB are treated as binaries
/// and left alone. PHP archives get their signature recomputed after
/// patching. Returns whether the file changed.
pub(crate) fn rewrite_text_file(path: &Path, replacements: &[(&str, &str)]) -> io::Result<bool> {
    if is_binary(path)? {
        return Ok(false);
    }

    let mut content = fs::read(path)?;
    let mut changed = false;
    for (from, to) in replacements {
        if from == to {
            continue;
        }
        if let Some(replaced) = replace_all(&content, from.as_bytes(), to.as_bytes()) {
            content = replaced;
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }
    if let Some(resigned) = phar::resign(&content) {
        content = resigned;
    }

    write_preserving_mode(path, &content)?;
    Ok(true)
}

/// Whether `path` is a binary file: it has a NUL byte in its first 8 KiB and
/// is not a `#!` script. This is the split Homebrew uses between files it
/// rewrites freely and files whose strings must be replaced in place.
pub(crate) fn is_binary(path: &Path) -> io::Result<bool> {
    let mut head = [0u8; 8192];
    let n = fs::File::open(path)?.read(&mut head)?;
    Ok(head[..n].contains(&0) && !head[..n].starts_with(b"#!"))
}

fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    let mut rest = haystack;
    let mut out = Vec::new();
    let mut found = false;
    while let Some(i) = find(rest, from) {
        found = true;
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(to);
        rest = &rest[i + from.len()..];
    }
    if !found {
        return None;
    }
    out.extend_from_slice(rest);
    Some(out)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn write_preserving_mode(path: &Path, content: &[u8]) -> io::Result<()> {
    let permissions = fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    let readonly = mode & 0o200 == 0;
    if readonly {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))?;
    }
    let result = fs::write(path, content);
    if readonly {
        fs::set_permissions(path, permissions)?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PLACEHOLDERS: &[(&str, &str)] = &[
        ("@@HOMEBREW_PREFIX@@", "/opt/zerobrew"),
        ("/opt/homebrew", "/opt/zerobrew"),
    ];

    #[test]
    fn rewrites_placeholders_and_prefix_in_text() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("script.sh");
        fs::write(
            &file,
            "#!/bin/sh\nA=@@HOMEBREW_PREFIX@@/bin\nB=/opt/homebrew/etc\n",
        )
        .unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "#!/bin/sh\nA=/opt/zerobrew/bin\nB=/opt/zerobrew/etc\n"
        );
    }

    #[test]
    fn rewrites_scripts_with_binary_payloads() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("tool.phar");
        fs::write(
            &file,
            b"#!/usr/bin/env php\n\0\x01\xc9'@@HOMEBREW_PREFIX@@/etc/cert.pem'",
        )
        .unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(
            fs::read(&file).unwrap(),
            b"#!/usr/bin/env php\n\0\x01\xc9'/opt/zerobrew/etc/cert.pem'"
        );
    }

    #[test]
    fn rewrites_text_that_is_not_utf8() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("latin1.conf");
        fs::write(&file, b"# caf\xe9\nprefix=@@HOMEBREW_PREFIX@@\n").unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(
            fs::read(&file).unwrap(),
            b"# caf\xe9\nprefix=/opt/zerobrew\n"
        );
    }

    #[test]
    fn leaves_binaries_alone() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("libfoo.a");
        let original = b"!<arch>\n\0\0@@HOMEBREW_PREFIX@@/lib".to_vec();
        fs::write(&file, &original).unwrap();

        assert!(!rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(fs::read(&file).unwrap(), original);
    }

    #[test]
    fn preserves_read_only_and_executable_modes() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("tool");
        fs::write(&file, "#!/bin/sh\nexec @@HOMEBREW_PREFIX@@/bin/real\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o555)).unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555);
    }

    #[test]
    fn reports_unchanged_files() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("README");
        fs::write(&file, "nothing to see\n").unwrap();
        assert!(!rewrite_text_file(&file, PLACEHOLDERS).unwrap());
    }

    #[test]
    fn resigns_patched_phars() {
        use sha2::{Digest, Sha512};

        let body =
            b"#!/usr/bin/env php\n<?php __HALT_COMPILER(); ?>\0'@@HOMEBREW_PREFIX@@/etc'".to_vec();
        let mut data = body.clone();
        data.extend_from_slice(&Sha512::digest(&body));
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(b"GBMB");

        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("composer");
        fs::write(&file, &data).unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        let patched = fs::read(&file).unwrap();
        let body_len = patched.len() - 8 - 64;
        assert_eq!(
            &patched[body_len..patched.len() - 8],
            &Sha512::digest(&patched[..body_len])[..]
        );
        assert!(patched.windows(13).any(|w| w == b"/opt/zerobrew"));
    }
}
