//! Keep PHP-serialized data valid after a keg's paths are rewritten.
//!
//! PEAR stores its config and package registry as `serialize()` output, where
//! every string carries its byte length: `s:48:"/opt/homebrew/..."`. Bottles
//! ship those files with `@@HOMEBREW_CELLAR@@` placeholders whose lengths were
//! computed for Homebrew's prefix, so replacing a placeholder with a prefix of
//! a different length leaves the counts wrong and `unserialize()` fails. PHP
//! then reports "The default config file is not a valid config file or is
//! corrupted" and every `pear` call exits non-zero.

/// If `data` is PHP-serialized and any string's declared length disagrees with
/// its contents, return a copy with the lengths corrected. `None` for anything
/// else, including data that is already consistent.
pub(crate) fn fix_lengths(data: &[u8]) -> Option<Vec<u8>> {
    if !looks_serialized(data) {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    let mut fixed = false;
    while i < data.len() {
        // `s:` only starts a string at the top or right after a separator,
        // which is also what keeps us out of a string's contents.
        let starts_value = i == 0 || matches!(data[i - 1], b'{' | b';' | b':' | b'\n');
        let Some((declared, open)) = starts_value
            .then(|| header(data, i))
            .flatten()
            .filter(|&(_, open)| open <= data.len())
        else {
            out.push(data[i]);
            i += 1;
            continue;
        };
        // The declared length is only trusted when the quote and separator
        // land where it says; otherwise the string ends at the first `";`
        // or `"}`. PEAR's values are paths and identifiers, never quotes.
        let end = match data.get(open + declared..) {
            Some([b'"', b';' | b'}', ..]) => open + declared,
            _ => match terminator(data, open) {
                Some(end) => end,
                None => {
                    out.push(data[i]);
                    i += 1;
                    continue;
                }
            },
        };
        let actual = end - open;
        if actual != declared {
            fixed = true;
        }
        out.extend_from_slice(format!("s:{actual}:\"").as_bytes());
        out.extend_from_slice(&data[open..end]);
        out.push(b'"');
        i = end + 1;
    }
    fixed.then_some(out)
}

/// `serialize()` output, optionally behind a `#PEAR_Config` comment line.
fn looks_serialized(data: &[u8]) -> bool {
    let body = match data.first() {
        Some(b'#') => match data.iter().position(|&b| b == b'\n') {
            Some(nl) => &data[nl + 1..],
            None => return false,
        },
        _ => data,
    };
    matches!(body.first(), Some(b'a' | b'O' | b's'))
        && matches!(body.get(1), Some(b':'))
        && body.get(2).is_some_and(u8::is_ascii_digit)
}

/// The declared length of the string starting at `i`, and the offset of its
/// first content byte, for `s:<digits>:"`.
fn header(data: &[u8], i: usize) -> Option<(usize, usize)> {
    if data.get(i) != Some(&b's') || data.get(i + 1) != Some(&b':') {
        return None;
    }
    let digits_at = i + 2;
    let digits = data[digits_at..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 || digits > 10 {
        return None;
    }
    let after = digits_at + digits;
    if data.get(after) != Some(&b':') || data.get(after + 1) != Some(&b'"') {
        return None;
    }
    let declared = std::str::from_utf8(&data[digits_at..after])
        .ok()?
        .parse()
        .ok()?;
    Some((declared, after + 2))
}

/// Where the string opening at `open` ends: the first `"` that a separator
/// follows.
fn terminator(data: &[u8], open: usize) -> Option<usize> {
    (open..data.len())
        .find(|&end| data[end] == b'"' && matches!(data.get(end + 1), Some(b';' | b'}') | None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_a_pear_config_shortened_by_relocation() {
        // What a bottle ships, with the length of the Homebrew path.
        let shipped = br#"#PEAR_Config 0.9
a:2:{s:7:"php_dir";s:48:"@@HOMEBREW_CELLAR@@/php/8.5.12_1/share/php/pear";s:13:"auto_discover";i:0;}"#;
        let relocated = String::from_utf8_lossy(shipped)
            .replace("@@HOMEBREW_CELLAR@@", "/tmp/test2/Cellar")
            .into_bytes();
        let fixed = fix_lengths(&relocated).expect("lengths were left wrong");
        let text = String::from_utf8(fixed).unwrap();
        assert!(
            text.contains(r#"s:45:"/tmp/test2/Cellar/php/8.5.12_1/share/php/pear""#),
            "{text}"
        );
        // Untouched entries keep their lengths.
        assert!(text.contains(r#"s:13:"auto_discover";i:0;}"#), "{text}");
    }

    #[test]
    fn a_longer_prefix_is_repaired_too() {
        let data = br#"a:1:{s:4:"path";s:10:"/opt/x";}"#;
        let text = String::from_utf8(fix_lengths(data).unwrap()).unwrap();
        assert_eq!(text, r#"a:1:{s:4:"path";s:6:"/opt/x";}"#);
    }

    #[test]
    fn consistent_data_is_left_alone() {
        assert!(fix_lengths(br#"a:1:{s:4:"path";s:6:"/opt/x";}"#).is_none());
        // A string whose contents look like a header is not descended into.
        assert!(fix_lengths(br#"a:1:{s:1:"k";s:8:"s:3:"ab"";}"#).is_none());
    }

    #[test]
    fn other_files_are_not_touched() {
        assert!(fix_lengths(b"#!/bin/sh\necho s:3:\"abc\"\n").is_none());
        assert!(fix_lengths(b"").is_none());
        assert!(fix_lengths(b"zend_extension=\"opcache.so\"\n").is_none());
    }
}
