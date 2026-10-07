//! Make a poured bottle run from the zerobrew prefix.
//!
//! Bottles are built for Homebrew's prefix. Relocatable ones (`:any`) carry
//! `@@HOMEBREW_PREFIX@@` and `@@HOMEBREW_CELLAR@@` placeholders in their text
//! files and Mach-O load commands; pinned ones carry the literal build prefix
//! in binaries too. Every file in the keg is read once and the changed bytes
//! are written back in place, so the keg keeps sharing unchanged blocks with
//! the store entry it was cloned from. Mach-O files whose bytes changed, or
//! that arrived unsigned, are ad-hoc signed in one `codesign` run per keg.

use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use memchr::memmem;
use rayon::prelude::*;
use regex::Regex;
use tracing::warn;
use zb_core::Error;

use super::macho::{self, MachoError, PathKind, UnfitChange};
use super::text;

/// Whether `path` is a Mach-O file, judging by its magic number.
pub(crate) fn is_macho(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && macho::has_macho_magic(&magic)
}

/// Extended attributes macOS adds to downloaded files and that make
/// Gatekeeper refuse to run them. Our downloads never get them, but a keg
/// that does would be unusable, so they are stripped like Homebrew does.
const QUARANTINE_XATTRS: &[&str] = &["com.apple.quarantine", "com.apple.provenance"];

/// The paths a keg has to be rewritten with.
#[derive(Debug, Clone)]
pub(crate) struct Relocation {
    build_prefix: String,
    prefix: String,
    cellar: String,
    library: String,
    version_fix: Option<VersionFix>,
}

/// A bottle can reference another version of its own formula, for example
/// after a rebuild; those paths are pointed at the version being installed.
#[derive(Debug, Clone)]
struct VersionFix {
    regex: Regex,
    version: String,
    replacement: String,
}

impl Relocation {
    pub(crate) fn new(
        cellar_dir: &Path,
        pkg_name: &str,
        pkg_version: &str,
        build_prefix: &str,
    ) -> Self {
        // cellar_dir is prefix/Cellar.
        let prefix = cellar_dir.parent().unwrap_or(Path::new("/opt/homebrew"));
        let prefix = prefix.to_string_lossy().into_owned();
        let version_fix = Regex::new(&format!(
            r"(/Cellar/{}/)([^/]+)(/)",
            regex::escape(pkg_name)
        ))
        .ok()
        .map(|regex| VersionFix {
            regex,
            version: pkg_version.to_string(),
            replacement: format!("/Cellar/{pkg_name}/{pkg_version}/"),
        });
        Self {
            build_prefix: build_prefix.to_string(),
            library: format!("{prefix}/Library"),
            cellar: cellar_dir.to_string_lossy().into_owned(),
            prefix,
            version_fix,
        }
    }

    /// The new value of a Mach-O load path, or `None` to keep it.
    fn load_path(&self, path: &str) -> Option<String> {
        let mut new = path.to_string();
        if new.contains("@@HOMEBREW_CELLAR@@") || new.contains("@@HOMEBREW_PREFIX@@") {
            new = new
                .replace("@@HOMEBREW_CELLAR@@", &self.cellar)
                .replace("@@HOMEBREW_PREFIX@@", &self.prefix);
        }
        // Pinned bottles keep the literal build prefix. The in-place pass
        // normally rewrote it already; this covers a prefix too long for that.
        if self.build_prefix != self.prefix
            && let Some(rest) = new.strip_prefix(&self.build_prefix)
            && rest.is_empty() | rest.starts_with('/')
        {
            new = format!("{}{rest}", self.prefix);
        }
        if let Some(fix) = &self.version_fix
            && fix.regex.is_match(&new)
        {
            new = fix
                .regex
                .replace(&new, |caps: &regex::Captures| {
                    if caps[2] == *fix.version {
                        caps[0].to_string()
                    } else {
                        fix.replacement.clone()
                    }
                })
                .into_owned();
        }
        (new != path).then_some(new)
    }

    fn text_replacements(&self) -> [(&str, &str); 7] {
        [
            ("@@HOMEBREW_PREFIX@@", self.prefix.as_str()),
            ("@@HOMEBREW_CELLAR@@", self.cellar.as_str()),
            ("@@HOMEBREW_REPOSITORY@@", self.prefix.as_str()),
            ("@@HOMEBREW_LIBRARY@@", self.library.as_str()),
            ("@@HOMEBREW_PERL@@", "/usr/bin/perl"),
            ("@@HOMEBREW_JAVA@@", "/usr/bin/java"),
            (self.build_prefix.as_str(), self.prefix.as_str()),
        ]
    }
}

/// A file whose mode was lifted so it could be written and signed.
struct Writable {
    path: PathBuf,
    mode: u32,
}

impl Writable {
    fn lift(path: &Path, metadata: &fs::Metadata) -> Result<Self, Error> {
        let mode = metadata.permissions().mode();
        if mode & 0o200 == 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))
                .map_err(Error::store("failed to make writable"))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            mode,
        })
    }

    fn restore(&self) -> Result<(), Error> {
        if self.mode & 0o200 == 0 {
            fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode))
                .map_err(Error::store("failed to restore permissions"))?;
        }
        Ok(())
    }
}

/// What relocating one binary file left to do.
enum BinaryOutcome {
    /// Nothing changed and nothing needs signing.
    Untouched,
    /// The file was written and may need signing; its mode is restored once
    /// signing is done.
    Changed { writable: Writable, sign: bool },
}

/// Replace `old` with `new` wherever it starts a path in `contents`: it is
/// followed by the end of the data, a NUL, a `/` or a `:`. A shorter `new`
/// is padded with `/` to keep the file layout, as Homebrew does, so the rest
/// of the path is still reachable. Returns the byte ranges that changed.
fn replace_prefix_in_place(contents: &mut [u8], old: &[u8], new: &[u8]) -> Vec<Range<usize>> {
    debug_assert!(new.len() <= old.len());
    let finder = memmem::Finder::new(old);
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(found) = finder.find(&contents[from..]) {
        let start = from + found;
        let end = start + old.len();
        let boundary = matches!(contents.get(end), None | Some(0) | Some(b'/') | Some(b':'));
        if boundary {
            contents[start..start + new.len()].copy_from_slice(new);
            contents[start + new.len()..end].fill(b'/');
            ranges.push(start..end);
            from = end;
        } else {
            from = start + 1;
        }
    }
    ranges
}

/// Whether `contents` has a path under `prefix` that could not be rewritten.
fn has_paths_under(contents: &[u8], prefix: &[u8]) -> bool {
    memmem::find_iter(contents, prefix)
        .any(|start| contents.get(start + prefix.len()) == Some(&b'/'))
}

/// Rewrite `build_prefix` paths and Mach-O load paths in one binary file,
/// writing only the changed bytes back.
fn relocate_binary(path: &Path, relocation: &Relocation) -> Result<BinaryOutcome, Error> {
    let metadata = fs::metadata(path).map_err(Error::store("failed to read metadata"))?;
    let mut contents = fs::read(path).map_err(Error::store("failed to read file"))?;
    let mut written = Vec::new();

    let old = relocation.build_prefix.as_bytes();
    let new = relocation.prefix.as_bytes();
    if old != new {
        if new.len() <= old.len() {
            written.extend(replace_prefix_in_place(&mut contents, old, new));
        } else if has_paths_under(&contents, old) {
            // Longer paths can't be written in place. The planner refuses to
            // pour bottles pinned to a shorter prefix, so this only happens
            // for relocatable bottles with stray references to it.
            //
            // See: https://github.com/zerobrewhq/zerobrew/issues/286
            warn!(
                path = %path.display(),
                old_prefix = %relocation.build_prefix,
                new_prefix = %relocation.prefix,
                "binary contains hardcoded paths under {} that could not be \
                 rewritten to {} (new path is longer); this package may not \
                 work correctly. tracking issue: \
                 https://github.com/zerobrewhq/zerobrew/issues/286",
                relocation.build_prefix,
                relocation.prefix,
            );
        }
    }

    let is_macho = macho::has_macho_magic(&contents);
    let mut unfit = Vec::new();
    let mut signed = true;
    if is_macho {
        match macho::rewrite(&mut contents, |_, current| relocation.load_path(current)) {
            Ok(rewrite) => {
                written.extend(rewrite.written);
                unfit = rewrite.unfit;
                signed = rewrite.signed;
            }
            // A Java class file starts with the fat magic; leave it alone.
            Err(MachoError::NotMacho) => {
                return finish_untouched(path, &written, &contents, &metadata);
            }
            Err(e) => warn!(
                path = %path.display(),
                error = %e,
                "cannot read Mach-O load commands; leaving them unchanged"
            ),
        }
    }

    let needs_signature = is_macho && !signed;
    if written.is_empty() && unfit.is_empty() && !needs_signature {
        return Ok(BinaryOutcome::Untouched);
    }

    let writable = Writable::lift(path, &metadata)?;
    write_ranges(path, &contents, &written)?;
    if !unfit.is_empty() {
        change_with_install_name_tool(path, &unfit)?;
    }

    Ok(BinaryOutcome::Changed {
        writable,
        sign: is_macho,
    })
}

/// A file with Mach-O-looking magic that is not Mach-O still gets its
/// literal prefix rewritten, but is never signed.
fn finish_untouched(
    path: &Path,
    written: &[Range<usize>],
    contents: &[u8],
    metadata: &fs::Metadata,
) -> Result<BinaryOutcome, Error> {
    if written.is_empty() {
        return Ok(BinaryOutcome::Untouched);
    }
    let writable = Writable::lift(path, metadata)?;
    write_ranges(path, contents, written)?;
    Ok(BinaryOutcome::Changed {
        writable,
        sign: false,
    })
}

fn write_ranges(path: &Path, contents: &[u8], ranges: &[Range<usize>]) -> Result<(), Error> {
    if ranges.is_empty() {
        return Ok(());
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(Error::store("failed to open file for patching"))?;
    for range in ranges {
        file.write_all_at(&contents[range.clone()], range.start as u64)
            .map_err(Error::store("failed to write patched bytes"))?;
    }
    Ok(())
}

/// Apply the load-path changes that did not fit in place. `install_name_tool`
/// can grow a load command into the header padding; one run handles every
/// change in the file.
fn change_with_install_name_tool(path: &Path, changes: &[UnfitChange]) -> Result<(), Error> {
    let mut command = Command::new("install_name_tool");
    for change in changes {
        match change.kind {
            PathKind::InstallName => command.args(["-id", &change.new]),
            PathKind::Dylib => command.args(["-change", &change.old, &change.new]),
            PathKind::Rpath => command.args(["-rpath", &change.old, &change.new]),
        };
    }
    let output = command
        .arg(path)
        .output()
        .map_err(Error::store("failed to run install_name_tool"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::StoreCorruption {
        message: format!(
            "install_name_tool failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    })
}

/// Ad-hoc sign files whose signature was invalidated by patching, keeping
/// their entitlements, requirements, flags and hardened runtime. One
/// `codesign` run signs a whole batch; if that fails, each file is signed on
/// its own so the one at fault can be reported. A file that still fails is
/// retried once on a fresh copy, which is how Homebrew works around a
/// codesign bug.
fn sign(paths: &[&Path]) {
    const BATCH: usize = 200;
    for batch in paths.chunks(BATCH) {
        if codesign(batch).is_ok() {
            continue;
        }
        for path in batch {
            if let Err(e) = codesign(&[path]).or_else(|_| resign_fresh_copy(path)) {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to re-sign patched file; it may be killed when run"
                );
            }
        }
    }
}

fn codesign(paths: &[&Path]) -> Result<(), String> {
    let output = Command::new("codesign")
        .args([
            "--sign",
            "-",
            "--force",
            "--preserve-metadata=entitlements,requirements,flags,runtime",
        ])
        .args(paths)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn resign_fresh_copy(path: &Path) -> Result<(), String> {
    let mut copy = path.as_os_str().to_owned();
    copy.push(".zb-resign");
    fs::copy(path, &copy).map_err(|e| e.to_string())?;
    fs::rename(&copy, path).map_err(|e| e.to_string())?;
    codesign(&[path])
}

/// Remove the quarantine attributes from one path, without following
/// symlinks. Paths without them are the common case and cost one failed
/// syscall each.
fn strip_quarantine(path: &Path) {
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    for name in QUARANTINE_XATTRS {
        let Ok(c_name) = CString::new(*name) else {
            continue;
        };
        // SAFETY: both strings are valid NUL-terminated C strings that
        // outlive the call; removexattr only reads them.
        unsafe {
            libc::removexattr(c_path.as_ptr(), c_name.as_ptr(), libc::XATTR_NOFOLLOW);
        }
    }
}

/// Rewrite every placeholder and build-prefix path in a keg so it runs from
/// this prefix, sign what changed, and strip quarantine attributes.
pub fn relocate_keg(
    keg_path: &Path,
    cellar_dir: &Path,
    pkg_name: &str,
    pkg_version: &str,
    build_prefix: &str,
) -> Result<(), Error> {
    let relocation = Relocation::new(cellar_dir, pkg_name, pkg_version, build_prefix);

    // One walk: strip quarantine from everything, and sort regular files by
    // the first bytes so each is read once below.
    let mut binaries = Vec::new();
    let mut texts = Vec::new();
    for entry in walkdir::WalkDir::new(keg_path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        strip_quarantine(entry.path());
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        match text::is_binary(&path) {
            Ok(true) => binaries.push(path),
            Ok(false) => texts.push(path),
            Err(e) => warn!(path = %path.display(), error = %e, "cannot read file; skipping"),
        }
    }

    let (changed, failures): (Vec<_>, Vec<_>) = binaries
        .par_iter()
        .map(|path| relocate_binary(path, &relocation))
        .partition(Result::is_ok);
    let changed: Vec<(Writable, bool)> = changed
        .into_iter()
        .filter_map(|outcome| match outcome {
            Ok(BinaryOutcome::Changed { writable, sign }) => Some((writable, sign)),
            _ => None,
        })
        .collect();

    let to_sign: Vec<&Path> = changed
        .iter()
        .filter(|(_, sign)| *sign)
        .map(|(writable, _)| writable.path.as_path())
        .collect();
    sign(&to_sign);
    for (writable, _) in &changed {
        writable.restore()?;
    }

    if let Some(Err(first)) = failures.first() {
        return Err(Error::StoreCorruption {
            message: format!(
                "failed to relocate {} binary files in {} (first failure: {first})",
                failures.len(),
                keg_path.display()
            ),
        });
    }

    let replacements = relocation.text_replacements();
    texts.par_iter().for_each(|path| {
        if let Err(e) = text::rewrite_text(path, &replacements) {
            warn!(path = %path.display(), error = %e, "failed to patch text file");
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn patch_binary_strings(
        path: &Path,
        build_prefix: &str,
        new_prefix: &str,
    ) -> Result<(), Error> {
        let cellar = Path::new(new_prefix).join("Cellar");
        let relocation = Relocation::new(&cellar, "pkg", "1.0", build_prefix);
        relocate_binary(path, &relocation).map(|_| ())
    }

    fn patch_text_file_strings(
        path: &Path,
        build_prefix: &str,
        _new_prefix: &str,
        new_cellar: &str,
    ) -> Result<(), Error> {
        let relocation = Relocation::new(Path::new(new_cellar), "pkg", "1.0", build_prefix);
        text::rewrite_text_file(path, &relocation.text_replacements())
            .map_err(Error::store("failed to patch text file"))?;
        Ok(())
    }

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

        // Not a real Mach-O (no load commands), so nothing else can be
        // rewritten either: skip rather than error.
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
    fn untouched_files_keep_their_mode_and_are_not_rewritten() {
        let tmp = TempDir::new().unwrap();
        let test_file = tmp.path().join("libfoo.a");
        fs::write(&test_file, b"!<arch>\n\0\0/usr/lib/libz.dylib\0").unwrap();
        fs::set_permissions(&test_file, fs::Permissions::from_mode(0o444)).unwrap();
        let before = fs::metadata(&test_file).unwrap().modified().unwrap();

        let relocation = Relocation::new(
            Path::new("/opt/zerobrew/Cellar"),
            "foo",
            "1.0",
            "/opt/homebrew",
        );
        assert!(matches!(
            relocate_binary(&test_file, &relocation).unwrap(),
            BinaryOutcome::Untouched
        ));

        let metadata = fs::metadata(&test_file).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o444);
        assert_eq!(metadata.modified().unwrap(), before);
    }

    #[test]
    fn replace_prefix_in_place_only_matches_whole_path_components() {
        let mut data =
            b"/opt/homebrew/lib\0/opt/homebrewery\0/opt/homebrew:/x\0/opt/homebrew".to_vec();

        let ranges = replace_prefix_in_place(&mut data, b"/opt/homebrew", b"/opt/zb");

        assert_eq!(ranges.len(), 3);
        assert_eq!(
            data,
            b"/opt/zb///////lib\0/opt/homebrewery\0/opt/zb//////:/x\0/opt/zb//////".to_vec()
        );
    }

    #[test]
    fn load_path_replaces_placeholders_and_fixes_versions() {
        let relocation = Relocation::new(
            Path::new("/opt/zerobrew/Cellar"),
            "mpdecimal",
            "4.0.1",
            "/opt/homebrew",
        );

        assert_eq!(
            relocation.load_path("@@HOMEBREW_PREFIX@@/opt/mpdecimal/lib/libmpdec.4.dylib"),
            Some("/opt/zerobrew/opt/mpdecimal/lib/libmpdec.4.dylib".into())
        );
        assert_eq!(
            relocation.load_path("@@HOMEBREW_CELLAR@@/mpdecimal/3.9.0/lib/libmpdec.4.dylib"),
            Some("/opt/zerobrew/Cellar/mpdecimal/4.0.1/lib/libmpdec.4.dylib".into())
        );
        assert_eq!(
            relocation.load_path("/opt/zerobrew/Cellar/mpdecimal/4.0.1/lib/libmpdec.4.dylib"),
            None
        );
        assert_eq!(
            relocation.load_path("/opt/zerobrew/Cellar/other/3.9.0/lib/libother.dylib"),
            None,
            "only this package's versions are fixed"
        );
        assert_eq!(relocation.load_path("/usr/lib/libSystem.B.dylib"), None);
        assert_eq!(relocation.load_path("@loader_path/../lib"), None);
    }

    #[test]
    fn load_path_rewrites_the_literal_build_prefix() {
        let relocation = Relocation::new(
            Path::new("/opt/zerobrew/prefix/Cellar"),
            "git",
            "2.0",
            "/opt/homebrew",
        );

        assert_eq!(
            relocation.load_path("/opt/homebrew/opt/pcre2/lib/libpcre2-8.0.dylib"),
            Some("/opt/zerobrew/prefix/opt/pcre2/lib/libpcre2-8.0.dylib".into())
        );
        assert_eq!(
            relocation.load_path("/opt/homebrewery/lib/libx.dylib"),
            None,
            "a longer directory name is not the prefix"
        );
        assert_eq!(relocation.load_path("/usr/local/lib/libx.dylib"), None);
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
            "#!/bin/sh\nPATH=/usr/local/bin:@@HOMEBREW_PREFIX@@/bin\n"
                .replace("@@HOMEBREW_PREFIX@@", "/opt/zerobrew")
        );
    }

    /// Compile with the system C compiler, or `None` when there is none.
    fn cc(args: &[&str]) -> Option<()> {
        let output = Command::new("cc").args(args).output().ok()?;
        if output.status.success() {
            Some(())
        } else {
            eprintln!("cc failed: {}", String::from_utf8_lossy(&output.stderr));
            None
        }
    }

    fn otool(args: &[&str], path: &Path) -> String {
        let output = Command::new("otool").args(args).arg(path).output().unwrap();
        assert!(
            output.status.success(),
            "otool failed on {}",
            path.display()
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn signature_is_valid(path: &Path) -> bool {
        Command::new("codesign")
            .arg("-v")
            .arg(path)
            .status()
            .unwrap()
            .success()
    }

    /// A keg as a bottle ships it: a library with placeholder install name
    /// and rpath, an executable loading it, a script and a config file. All
    /// files are read-only, like extracted bottle files.
    struct BuiltKeg {
        keg: PathBuf,
        dylib: PathBuf,
        app: PathBuf,
    }

    fn build_keg(prefix: &Path, install_name: &str, rpath: &str) -> Option<BuiltKeg> {
        let keg = prefix.join("Cellar/x/1.0");
        fs::create_dir_all(keg.join("lib")).unwrap();
        fs::create_dir_all(keg.join("bin")).unwrap();
        fs::create_dir_all(keg.join("etc")).unwrap();
        let src = prefix.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("x.c"), "int x(void) { return 42; }\n").unwrap();
        fs::write(
            src.join("app.c"),
            "#include <stdio.h>\nint x(void);\nint main(void) { printf(\"%d\\n\", x()); return 0; }\n",
        )
        .unwrap();

        let dylib = keg.join("lib/libx.dylib");
        let app = keg.join("bin/app");
        // Homebrew links with headerpad so install names can grow.
        cc(&[
            "-dynamiclib",
            "-Wl,-headerpad_max_install_names",
            "-o",
            dylib.to_str().unwrap(),
            "-install_name",
            install_name,
            &format!("-Wl,-rpath,{rpath}"),
            src.join("x.c").to_str().unwrap(),
        ])?;
        cc(&[
            "-Wl,-headerpad_max_install_names",
            "-o",
            app.to_str().unwrap(),
            src.join("app.c").to_str().unwrap(),
            dylib.to_str().unwrap(),
            &format!("-Wl,-rpath,{rpath}"),
        ])?;
        fs::write(
            keg.join("bin/x-config"),
            "#!/bin/sh\necho @@HOMEBREW_PREFIX@@/opt/x\n",
        )
        .unwrap();
        fs::write(keg.join("etc/x.conf"), "root = @@HOMEBREW_CELLAR@@/x/1.0\n").unwrap();
        for entry in walkdir::WalkDir::new(&keg)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                let mode = if entry.path().starts_with(keg.join("bin")) {
                    0o555
                } else {
                    0o444
                };
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        // What Homebrew's `opt/` link would provide at run time.
        fs::create_dir_all(prefix.join("opt")).unwrap();
        std::os::unix::fs::symlink(&keg, prefix.join("opt/x")).unwrap();
        Some(BuiltKeg { keg, dylib, app })
    }

    fn run_app(app: &Path) -> String {
        let output = Command::new(app).output().unwrap();
        assert!(
            output.status.success(),
            "{} failed: {}",
            app.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A prefix short enough for every placeholder to be rewritten in place,
    /// so no `install_name_tool` is needed.
    fn short_prefix() -> TempDir {
        tempfile::Builder::new()
            .prefix("zb")
            .tempdir_in("/private/tmp")
            .unwrap()
    }

    #[test]
    fn relocates_a_real_keg_in_place_and_it_runs() {
        let tmp = short_prefix();
        let prefix = tmp.path();
        let Some(built) = build_keg(
            prefix,
            "@@HOMEBREW_PREFIX@@/opt/x/lib/libx.dylib",
            "@@HOMEBREW_PREFIX@@/lib",
        ) else {
            eprintln!("skipping: no working C compiler");
            return;
        };
        let cellar = prefix.join("Cellar");
        let relocation = Relocation::new(&cellar, "x", "1.0", "/opt/homebrew");

        // Every change fits in the load commands, so nothing needs the tool.
        let mut bytes = fs::read(&built.dylib).unwrap();
        let rewrite = macho::rewrite(&mut bytes, |_, p| relocation.load_path(p)).unwrap();
        assert!(rewrite.unfit.is_empty(), "{:?}", rewrite.unfit);
        assert_eq!(rewrite.written.len(), 2, "install name and rpath");

        relocate_keg(&built.keg, &cellar, "x", "1.0", "/opt/homebrew").unwrap();

        let want_lib = format!("{}/opt/x/lib/libx.dylib", prefix.display());
        let dylib_cmds = otool(&["-l"], &built.dylib);
        assert!(
            dylib_cmds.contains(&format!("name {want_lib}")),
            "{dylib_cmds}"
        );
        assert!(
            dylib_cmds.contains(&format!("path {}/lib", prefix.display())),
            "rpath not rewritten:\n{dylib_cmds}"
        );
        assert!(!dylib_cmds.contains("@@HOMEBREW"), "{dylib_cmds}");
        let app_cmds = otool(&["-L"], &built.app);
        assert!(app_cmds.contains(&want_lib), "{app_cmds}");

        assert!(signature_is_valid(&built.dylib));
        assert!(signature_is_valid(&built.app));
        assert_eq!(run_app(&built.app), "42");

        // Modes survive the rewrite and the signing.
        assert_eq!(
            fs::metadata(&built.dylib).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert_eq!(
            fs::metadata(&built.app).unwrap().permissions().mode() & 0o777,
            0o555
        );

        // Text files are still patched.
        assert_eq!(
            fs::read_to_string(built.keg.join("bin/x-config")).unwrap(),
            format!("#!/bin/sh\necho {}/opt/x\n", prefix.display())
        );
        assert_eq!(
            fs::read_to_string(built.keg.join("etc/x.conf")).unwrap(),
            format!("root = {}/x/1.0\n", cellar.display())
        );
    }

    #[test]
    fn relocates_through_install_name_tool_when_paths_do_not_fit() {
        // A deep temp dir: the prefix is longer than the placeholders.
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("a-prefix-longer-than-any-placeholder");
        let Some(built) = build_keg(
            &prefix,
            "@@HOMEBREW_PREFIX@@/opt/x/lib/libx.dylib",
            "@@HOMEBREW_CELLAR@@/x/1.0/lib",
        ) else {
            eprintln!("skipping: no working C compiler");
            return;
        };
        let cellar = prefix.join("Cellar");
        let relocation = Relocation::new(&cellar, "x", "1.0", "/opt/homebrew");

        let mut bytes = fs::read(&built.dylib).unwrap();
        let rewrite = macho::rewrite(&mut bytes, |_, p| relocation.load_path(p)).unwrap();
        assert_eq!(rewrite.unfit.len(), 2, "both changes need the tool");

        relocate_keg(&built.keg, &cellar, "x", "1.0", "/opt/homebrew").unwrap();

        let want_lib = format!("{}/opt/x/lib/libx.dylib", prefix.display());
        let dylib_cmds = otool(&["-l"], &built.dylib);
        assert!(
            dylib_cmds.contains(&format!("name {want_lib}")),
            "{dylib_cmds}"
        );
        assert!(
            dylib_cmds.contains(&format!("path {}/x/1.0/lib", cellar.display())),
            "{dylib_cmds}"
        );
        assert!(otool(&["-L"], &built.app).contains(&want_lib));
        assert!(signature_is_valid(&built.dylib));
        assert!(signature_is_valid(&built.app));
        assert_eq!(run_app(&built.app), "42");
    }

    #[test]
    fn fixes_references_to_another_version_of_the_same_package() {
        let tmp = short_prefix();
        let prefix = tmp.path();
        let Some(built) = build_keg(
            prefix,
            "@@HOMEBREW_CELLAR@@/x/0.9/lib/libx.dylib",
            "@loader_path/../lib",
        ) else {
            eprintln!("skipping: no working C compiler");
            return;
        };
        let cellar = prefix.join("Cellar");

        relocate_keg(&built.keg, &cellar, "x", "1.0", "/opt/homebrew").unwrap();

        let want_lib = format!("{}/x/1.0/lib/libx.dylib", cellar.display());
        assert!(otool(&["-D"], &built.dylib).contains(&want_lib));
        assert!(otool(&["-L"], &built.app).contains(&want_lib));
        assert!(
            otool(&["-l"], &built.dylib).contains("path @loader_path/../lib"),
            "rpaths without placeholders are left alone"
        );
        assert_eq!(run_app(&built.app), "42");
    }

    #[test]
    fn signs_binaries_that_arrive_unsigned_even_when_nothing_changes() {
        let tmp = short_prefix();
        let prefix = tmp.path();
        let Some(built) = build_keg(prefix, "@rpath/libx.dylib", "@loader_path/../lib") else {
            eprintln!("skipping: no working C compiler");
            return;
        };
        for path in [&built.dylib, &built.app] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            let status = Command::new("codesign")
                .arg("--remove-signature")
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success());
            assert!(!signature_is_valid(path));
        }
        let before = fs::read(&built.dylib).unwrap();

        relocate_keg(
            &built.keg,
            &prefix.join("Cellar"),
            "x",
            "1.0",
            "/opt/homebrew",
        )
        .unwrap();

        assert!(signature_is_valid(&built.dylib));
        assert!(signature_is_valid(&built.app));
        assert_ne!(
            fs::read(&built.dylib).unwrap(),
            before,
            "a signature was added"
        );
        assert_eq!(run_app(&built.app), "42");
    }

    #[test]
    fn relocates_every_architecture_of_a_fat_binary() {
        let tmp = short_prefix();
        let prefix = tmp.path();
        let src = prefix.join("x.c");
        fs::write(&src, "int x(void) { return 42; }\n").unwrap();
        let keg = prefix.join("Cellar/x/1.0");
        fs::create_dir_all(keg.join("lib")).unwrap();
        let dylib = keg.join("lib/libx.dylib");
        if cc(&[
            "-arch",
            "arm64",
            "-arch",
            "x86_64",
            "-dynamiclib",
            "-o",
            dylib.to_str().unwrap(),
            "-install_name",
            "@@HOMEBREW_PREFIX@@/opt/x/lib/libx.dylib",
            src.to_str().unwrap(),
        ])
        .is_none()
        {
            eprintln!("skipping: cannot build a fat binary here");
            return;
        }
        let cellar = prefix.join("Cellar");

        relocate_keg(&keg, &cellar, "x", "1.0", "/opt/homebrew").unwrap();

        let ids = otool(&["-D"], &dylib);
        let want = format!("{}/opt/x/lib/libx.dylib", prefix.display());
        assert_eq!(
            ids.matches(&want).count(),
            2,
            "both slices must carry the new install name:\n{ids}"
        );
        assert!(signature_is_valid(&dylib));
    }

    #[test]
    fn strip_quarantine_removes_the_attribute() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("downloaded");
        fs::write(&file, b"data").unwrap();
        let status = Command::new("xattr")
            .args(["-w", "com.apple.quarantine", "0083;00000000;Safari;"])
            .arg(&file)
            .status()
            .unwrap();
        assert!(status.success());

        strip_quarantine(&file);

        let output = Command::new("xattr").arg(&file).output().unwrap();
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("com.apple.quarantine"),
            "attribute still present"
        );
        // Harmless on a file without the attribute, and on nothing at all.
        strip_quarantine(&file);
        strip_quarantine(&tmp.path().join("missing"));
    }
}
