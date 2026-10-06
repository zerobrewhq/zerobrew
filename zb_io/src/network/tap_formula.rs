use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use zb_core::formula::{
    Bottle, BottleFile, BottleStable, FormulaUrls, KegOnly, SourceUrl, Versions,
};
use zb_core::{Error, Formula};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapFormulaRef {
    pub owner: String,
    pub repo: String,
    pub formula: String,
}

static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*version\s+["']([^"']+)["']"#).expect("VERSION_RE must compile")
});
static URL_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?m)^\s*url\s+["'][^"']*(?:refs/tags|archive|download)/v?([0-9][0-9A-Za-z._+-]*)"#,
    )
    .expect("URL_VERSION_RE must compile")
});
static REVISION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*revision\s+(\d+)\s*$"#).expect("REVISION_RE must compile")
});
static DEPENDS_ON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*depends_on\s+["']([^"']+)["'](.*)$"#).expect("DEPENDS_ON_RE must compile")
});
static SOURCE_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*url\s+["']([^"']+)["']"#).expect("SOURCE_URL_RE must compile")
});
static SOURCE_SHA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*sha256\s+["']([0-9a-f]{64})["']\s*$"#)
        .expect("SOURCE_SHA_RE must compile")
});
static CLASS_START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*class\s+\w+\s*<\s*Formula\b"#).expect("CLASS_START_RE must compile")
});
static BOTTLE_START_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*bottle\s+do\b"#).expect("BOTTLE_START_RE must compile"));
static END_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*end\b"#).expect("END_RE must compile"));
static DO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bdo\b\s*(?:\|[^|]*\|\s*)?(?:#.*)?$"#).expect("DO_RE must compile")
});
static KEYWORD_START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(if|unless|case|begin|def|class|module|for|while|until)\b"#)
        .expect("KEYWORD_START_RE must compile")
});
static ROOT_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"root_url\s+["']([^"']+)["']"#).expect("ROOT_URL_RE must compile")
});
static REBUILD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*rebuild\s+(\d+)\s*$"#).expect("REBUILD_RE must compile")
});
static BOTTLE_SHA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([a-z0-9_]+):\s*"([0-9a-f]{64})""#).expect("BOTTLE_SHA_RE must compile")
});

static BOTTLE_CELLAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"cellar:\s*(:[a-z_]+|"[^"]+")"#).expect("BOTTLE_CELLAR_RE must compile")
});
static ON_PLATFORM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*on_(macos|linux|arm|intel)\s+do\b"#).expect("ON_PLATFORM_RE must compile")
});
static HW_CPU_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*if\s+Hardware::CPU\.(arm|intel)\?"#).expect("HW_CPU_RE must compile")
});
static ELSIF_HW_CPU_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*elsif\s+Hardware::CPU\.(arm|intel)\?"#).expect("ELSIF_HW_CPU_RE must compile")
});
static ELSE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*else\s*(?:#.*)?$"#).expect("ELSE_LINE_RE must compile"));

pub fn parse_tap_formula_ref(input: &str) -> Option<TapFormulaRef> {
    let mut parts = input.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let formula = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if owner.is_empty() || repo.is_empty() || formula.is_empty() {
        return None;
    }
    Some(TapFormulaRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
        formula: formula.to_string(),
    })
}

/// Pre-processes a tap formula Ruby source to resolve platform-conditional blocks
/// (`on_macos do`, `on_linux do`, `on_arm do`, `on_intel do`), architecture
/// conditionals (`if Hardware::CPU.arm?`, `if Hardware::CPU.intel?`), and Ruby
/// `#{version}` string interpolation so that the downstream regex-based parser
/// sees the relevant fields at the top level.
fn preprocess_tap_source(source: &str) -> String {
    let resolved = resolve_on_platform_blocks(source);
    let resolved = resolve_arch_conditionals(&resolved);
    resolve_version_interpolation(&resolved)
}

/// Returns `Some(true)` when the line opens a platform block that matches the
/// current compile target, `Some(false)` when it opens one that does not
/// match, and `None` when the line is not a platform block at all.
fn platform_block_matches(trimmed: &str) -> Option<bool> {
    let cap = ON_PLATFORM_RE.captures(trimmed)?;
    let platform = cap.get(1)?.as_str();
    Some(match platform {
        "macos" => cfg!(target_os = "macos"),
        "linux" => cfg!(target_os = "linux"),
        "arm" => cfg!(target_arch = "aarch64"),
        "intel" => cfg!(target_arch = "x86_64"),
        _ => false,
    })
}

/// Returns `Some(true)` when the line is an `if Hardware::CPU.{arm,intel}?`
/// conditional that matches the current architecture, `Some(false)` when it
/// does not match, and `None` when the line is not an arch conditional.
fn arch_conditional_matches(trimmed: &str) -> Option<bool> {
    let cap = HW_CPU_RE.captures(trimmed)?;
    let arch = cap.get(1)?.as_str();
    Some(match arch {
        "arm" => cfg!(target_arch = "aarch64"),
        "intel" => cfg!(target_arch = "x86_64"),
        _ => false,
    })
}

/// Returns `Some(true)` when the line is an `elsif Hardware::CPU.{arm,intel}?`
/// conditional that matches the current architecture, `Some(false)` when it
/// does not match, and `None` when the line is not an elsif arch conditional.
fn arch_conditional_matches_elsif(trimmed: &str) -> Option<bool> {
    let cap = ELSIF_HW_CPU_RE.captures(trimmed)?;
    let arch = cap.get(1)?.as_str();
    Some(match arch {
        "arm" => cfg!(target_arch = "aarch64"),
        "intel" => cfg!(target_arch = "x86_64"),
        _ => false,
    })
}

/// Counts how many Ruby blocks are opened by a single source line (via `do`
/// keywords and Ruby control-flow keywords like `if`, `def`, etc.).
fn count_block_opens(trimmed: &str) -> usize {
    let mut count = DO_RE.find_iter(trimmed).count();
    if KEYWORD_START_RE.is_match(trimmed) {
        count += 1;
    }
    count
}

/// Starting from `start` (the first line inside the block), finds the index
/// of the `end` line that closes the block opened immediately before `start`.
/// Returns `lines.len()` if no matching `end` is found.
fn find_matching_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 1usize;
    for (i, line) in lines.iter().enumerate().skip(start) {
        let trimmed = line.trim();
        if END_RE.is_match(trimmed) {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        } else {
            depth += count_block_opens(trimmed);
        }
    }
    lines.len()
}

/// A single branch from an `if … elsif … else` block. `lines` is the branch
/// body; `matches` is `Some(bool)` for if/elsif (whether the arch conditional
/// matches) and `None` for else.
type ArchBranch<'a> = (Vec<&'a str>, Option<bool>);

/// Splits the body of an `if … elsif … else … end` block into branches.
/// Returns `(branches, end_index)`.
fn split_if_else_elsif<'a>(
    lines: &[&'a str],
    start: usize,
    if_matches: bool,
) -> (Vec<ArchBranch<'a>>, usize) {
    let mut branches: Vec<ArchBranch<'a>> = Vec::new();
    let mut current_branch = Vec::new();
    let mut current_matches = Some(if_matches);
    let mut depth = 0usize;
    let mut i = start;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        if END_RE.is_match(trimmed) {
            if depth == 0 {
                branches.push((current_branch, current_matches));
                return (branches, i);
            }
            depth -= 1;
        } else if depth == 0 {
            if let Some(elsif_matches) = arch_conditional_matches_elsif(trimmed) {
                branches.push((current_branch, current_matches));
                current_branch = Vec::new();
                current_matches = Some(elsif_matches);
                i += 1;
                continue;
            } else if ELSE_LINE_RE.is_match(trimmed) {
                branches.push((current_branch, current_matches));
                current_branch = Vec::new();
                current_matches = None;
                i += 1;
                continue;
            }
            depth += count_block_opens(trimmed);
        } else {
            depth += count_block_opens(trimmed);
        }

        current_branch.push(lines[i]);
        i += 1;
    }

    branches.push((current_branch, current_matches));
    (branches, lines.len().saturating_sub(1))
}

/// Resolves `on_macos do`/`on_linux do`/`on_arm do`/`on_intel do` blocks.
/// Matching platform blocks are unwrapped (wrapper lines removed, content
/// kept). Non-matching platform blocks are removed entirely.
///
/// Inside `bottle do` blocks, all platform sub-blocks are unconditionally
/// unwrapped because the bottle block is a data declaration listing downloads
/// for every platform, not conditional code.
fn resolve_on_platform_blocks(source: &str) -> String {
    resolve_on_platform_blocks_inner(source, false)
}

fn resolve_on_platform_blocks_inner(source: &str, inside_bottle: bool) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Detect entering a `bottle do` block so nested platform blocks are
        // unwrapped unconditionally.
        if !inside_bottle && BOTTLE_START_RE.is_match(trimmed) {
            result.push(lines[i].to_string());
            let end_idx = find_matching_end(&lines, i + 1);
            let inner: String = lines[i + 1..end_idx.min(lines.len())].join("\n");
            let resolved = resolve_on_platform_blocks_inner(&inner, true);
            for line in resolved.lines() {
                result.push(line.to_string());
            }
            if end_idx < lines.len() {
                result.push(lines[end_idx].to_string());
                i = end_idx + 1;
            } else {
                i = lines.len();
            }
            continue;
        }

        if let Some(matches) = platform_block_matches(trimmed) {
            let end_idx = find_matching_end(&lines, i + 1);
            if inside_bottle || matches {
                let inner: String = lines[i + 1..end_idx.min(lines.len())].join("\n");
                let resolved = resolve_on_platform_blocks_inner(&inner, inside_bottle);
                for line in resolved.lines() {
                    result.push(line.to_string());
                }
            }
            i = if end_idx < lines.len() {
                end_idx + 1
            } else {
                lines.len()
            };
            continue;
        }

        result.push(lines[i].to_string());
        i += 1;
    }

    result.join("\n")
}

/// Resolves `if Hardware::CPU.arm?` / `if Hardware::CPU.intel?` conditionals,
/// including `elsif` branches. The first matching branch is kept; all others
/// are removed.
fn resolve_arch_conditionals(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        if let Some(if_matches) = arch_conditional_matches(trimmed) {
            let (branches, end_idx) = split_if_else_elsif(&lines, i + 1, if_matches);
            let branch = branches
                .into_iter()
                .find_map(|(lines, matches)| match matches {
                    Some(true) => Some(lines),
                    Some(false) => None,
                    None => Some(lines),
                })
                .unwrap_or_default();
            let branch_source = branch.join("\n");
            let resolved = resolve_arch_conditionals(&branch_source);
            for line in resolved.lines() {
                result.push(line.to_string());
            }
            i = if end_idx < lines.len() {
                end_idx + 1
            } else {
                lines.len()
            };
            continue;
        }

        result.push(lines[i].to_string());
        i += 1;
    }

    result.join("\n")
}

/// Replaces `#{version}` in the source with the actual version string extracted
/// from the `version "..."` directive, enabling correct URL resolution for tap
/// formulas that use Ruby string interpolation.
fn resolve_version_interpolation(source: &str) -> String {
    if let Some(version) = parse_version(source) {
        source.replace("#{version}", &version)
    } else {
        source.to_string()
    }
}

pub fn parse_tap_formula_ruby(spec: &TapFormulaRef, source: &str) -> Result<Formula, Error> {
    let source = preprocess_tap_source(source);
    let stable = parse_version(&source).unwrap_or_else(|| "0".to_string());
    let revision = parse_revision(&source).unwrap_or(0);
    let dependencies = parse_runtime_dependencies(&source);
    let build_dependencies = parse_build_dependencies(&source);
    let parsed_source_url = parse_source_url(&source);
    let bottle = parse_bottle(spec, &source, &stable, revision);

    let source_url = match parsed_source_url {
        ParsedSourceUrl::PresentWithChecksum(source_url) => Some(source_url),
        ParsedSourceUrl::PresentMissingChecksum => {
            if bottle.is_none() {
                return Err(Error::UnsupportedFormula {
                    name: spec.formula.clone(),
                    reason: "tap formula source url is missing sha256".to_string(),
                });
            }
            None
        }
        ParsedSourceUrl::NotPresent => None,
    };

    if bottle.is_none() && source_url.is_none() {
        return Err(Error::UnsupportedFormula {
            name: spec.formula.clone(),
            reason: "tap formula does not provide bottle data or source url".to_string(),
        });
    }

    Ok(Formula {
        name: spec.formula.clone(),
        versions: Versions { stable },
        dependencies,
        bottle: bottle.unwrap_or_else(empty_bottle),
        revision,
        keg_only: KegOnly::default(),
        keg_only_reason: None,
        build_dependencies,
        urls: source_url.map(|stable| FormulaUrls {
            stable: Some(stable),
            head: None,
        }),
        ruby_source_path: None,
        ruby_source_checksum: None,
        uses_from_macos: Vec::new(),
        requirements: Vec::new(),
        variations: None,
        aliases: Vec::new(),
        oldnames: Vec::new(),
    })
}

fn parse_version(source: &str) -> Option<String> {
    if let Some(v) = VERSION_RE
        .captures(source)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
    {
        return Some(v);
    }

    URL_VERSION_RE
        .captures(source)
        .and_then(|c| c.get(1))
        .map(|m| normalize_inferred_version(m.as_str()))
}

fn normalize_inferred_version(raw: &str) -> String {
    let mut v = raw.to_string();
    for suffix in [".tar.gz", ".tar.xz", ".tar.bz2", ".tgz", ".zip"] {
        if v.ends_with(suffix) {
            v.truncate(v.len() - suffix.len());
            break;
        }
    }
    v
}

fn parse_revision(source: &str) -> Option<u32> {
    REVISION_RE
        .captures(source)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u32>().ok())
}

fn parse_runtime_dependencies(source: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let body = extract_formula_class_body(source).unwrap_or(source);
    let mut depth = 0usize;

    for line in body.lines() {
        let trimmed = line.trim();
        if depth == 0
            && let Some(cap) = DEPENDS_ON_RE.captures(trimmed)
        {
            let options = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if !options.contains(":build")
                && !options.contains(":test")
                && let Some(dep) = cap.get(1)
            {
                deps.push(dep.as_str().to_string());
            }
        }
        update_depth(&mut depth, trimmed);
    }

    deps.sort_unstable();
    deps.dedup();
    deps
}

fn parse_build_dependencies(source: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let body = extract_formula_class_body(source).unwrap_or(source);
    let mut depth = 0usize;

    for line in body.lines() {
        let trimmed = line.trim();
        if depth == 0
            && let Some(cap) = DEPENDS_ON_RE.captures(trimmed)
        {
            let options = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if options.contains(":build")
                && let Some(dep) = cap.get(1)
            {
                deps.push(dep.as_str().to_string());
            }
        }
        update_depth(&mut depth, trimmed);
    }

    deps.sort_unstable();
    deps.dedup();
    deps
}

enum ParsedSourceUrl {
    NotPresent,
    PresentMissingChecksum,
    PresentWithChecksum(SourceUrl),
}

fn parse_source_url(source: &str) -> ParsedSourceUrl {
    let body = extract_formula_class_body(source).unwrap_or(source);
    let mut depth = 0usize;
    let mut url: Option<String> = None;
    let mut checksum: Option<String> = None;

    for line in body.lines() {
        let trimmed = line.trim();

        if depth == 0 {
            if url.is_none()
                && let Some(cap) = SOURCE_URL_RE.captures(trimmed)
            {
                url = cap.get(1).map(|m| m.as_str().to_string());
            }

            if checksum.is_none()
                && let Some(cap) = SOURCE_SHA_RE.captures(trimmed)
            {
                checksum = cap.get(1).map(|m| m.as_str().to_string());
            }

            if url.is_some() && checksum.is_some() {
                break;
            }
        }

        update_depth(&mut depth, trimmed);
    }

    match (url, checksum) {
        (Some(url), Some(checksum)) => ParsedSourceUrl::PresentWithChecksum(SourceUrl {
            url,
            checksum: Some(checksum),
            tag: None,
            revision: None,
        }),
        (Some(_), None) => ParsedSourceUrl::PresentMissingChecksum,
        _ => ParsedSourceUrl::NotPresent,
    }
}

fn update_depth(depth: &mut usize, trimmed: &str) {
    if END_RE.is_match(trimmed) {
        *depth = depth.saturating_sub(1);
        return;
    }

    *depth += DO_RE.find_iter(trimmed).count();
    if KEYWORD_START_RE.is_match(trimmed) {
        *depth += 1;
    }
}

fn extract_formula_class_body(source: &str) -> Option<&str> {
    let mut offset = 0usize;
    let mut class_body_start: Option<usize> = None;
    let mut depth = 0usize;

    for line in source.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim();

        if class_body_start.is_none() {
            if CLASS_START_RE.is_match(trimmed) {
                class_body_start = Some(offset);
                depth = 1;
            }
            continue;
        }

        let depth_before = depth;
        update_depth(&mut depth, trimmed);
        if depth_before > 0 && depth == 0 {
            return class_body_start.map(|start| &source[start..line_start]);
        }
    }

    None
}

fn parse_bottle(spec: &TapFormulaRef, source: &str, stable: &str, revision: u32) -> Option<Bottle> {
    let block = extract_bottle_block(source)?;

    let root_url = parse_root_url(block)
        .unwrap_or_else(|| format!("https://ghcr.io/v2/{}/{}", spec.owner, spec.repo));
    let rebuild = parse_rebuild(block).unwrap_or(0);
    let files = parse_bottle_files(spec, &root_url, stable, revision, rebuild, block);

    if files.is_empty() {
        return None;
    }

    Some(Bottle {
        stable: BottleStable { files, rebuild },
    })
}

fn empty_bottle() -> Bottle {
    Bottle {
        stable: BottleStable {
            files: BTreeMap::new(),
            rebuild: 0,
        },
    }
}

fn extract_bottle_block(source: &str) -> Option<&str> {
    let mut offset = 0usize;
    let mut bottle_body_start: Option<usize> = None;
    let mut depth = 0usize;

    for line in source.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim();

        if bottle_body_start.is_none() {
            if BOTTLE_START_RE.is_match(trimmed) {
                bottle_body_start = Some(offset);
                depth = 1;
            }
            continue;
        }

        let depth_before = depth;
        update_depth(&mut depth, trimmed);
        if depth_before > 0 && depth == 0 {
            return bottle_body_start.map(|start| &source[start..line_start]);
        }
    }

    None
}

fn parse_root_url(block: &str) -> Option<String> {
    ROOT_URL_RE
        .captures(block)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn parse_rebuild(block: &str) -> Option<u32> {
    REBUILD_RE
        .captures(block)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u32>().ok())
}

fn parse_bottle_files(
    spec: &TapFormulaRef,
    root_url: &str,
    stable: &str,
    revision: u32,
    rebuild: u32,
    block: &str,
) -> BTreeMap<String, BottleFile> {
    let mut files = BTreeMap::new();

    for line in block
        .lines()
        .filter(|line| line.trim_start().starts_with("sha256"))
    {
        let cellar = BOTTLE_CELLAR_RE
            .captures(line)
            .and_then(|cap| cap.get(1))
            .map(|m| m.as_str().trim_matches('"').to_string());

        for cap in BOTTLE_SHA_RE.captures_iter(line) {
            let Some(tag) = cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            let Some(sha) = cap.get(2).map(|m| m.as_str()) else {
                continue;
            };
            if tag == "cellar" {
                continue;
            }
            let url = build_bottle_url(spec, root_url, stable, revision, rebuild, tag, sha);
            files.insert(
                tag.to_string(),
                BottleFile {
                    url,
                    sha256: sha.to_string(),
                    cellar: Some(
                        cellar
                            .clone()
                            .unwrap_or_else(|| default_cellar(tag).to_string()),
                    ),
                },
            );
        }
    }

    files
}

/// The Cellar a bottle is pinned to when its `sha256` line has no `cellar:`,
/// matching Homebrew's `Tag#default_cellar`. `brew bottle` only writes
/// `cellar:` when it differs from this.
fn default_cellar(tag: &str) -> &'static str {
    if tag.ends_with("_linux") {
        "/home/linuxbrew/.linuxbrew/Cellar"
    } else if tag.starts_with("arm64_") {
        "/opt/homebrew/Cellar"
    } else {
        "/usr/local/Cellar"
    }
}

fn build_bottle_url(
    spec: &TapFormulaRef,
    root_url: &str,
    stable: &str,
    revision: u32,
    rebuild: u32,
    tag: &str,
    sha: &str,
) -> String {
    let normalized = root_url.trim_end_matches('/');
    if normalized.contains("/v2/") {
        return format!("{}/{}/blobs/sha256:{}", normalized, spec.formula, sha);
    }

    let effective_version = if revision > 0 {
        format!("{stable}_{revision}")
    } else {
        stable.to_string()
    };

    if rebuild > 0 {
        format!(
            "{}/{}-{}.{}.{}.bottle.tar.gz",
            normalized, spec.formula, effective_version, rebuild, tag
        )
    } else {
        format!(
            "{}/{}-{}.{}.bottle.tar.gz",
            normalized, spec.formula, effective_version, tag
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tap_formula_reference() {
        let parsed = parse_tap_formula_ref("hashicorp/tap/terraform").unwrap();
        assert_eq!(parsed.owner, "hashicorp");
        assert_eq!(parsed.repo, "tap");
        assert_eq!(parsed.formula, "terraform");
    }

    #[test]
    fn rejects_non_tap_reference() {
        assert!(parse_tap_formula_ref("jq").is_none());
        assert!(parse_tap_formula_ref("a/b").is_none());
        assert!(parse_tap_formula_ref("a/b/c/d").is_none());
    }

    #[test]
    fn parses_formula_subset_with_bottle_data() {
        let source = r#"
class Terraform < Formula
  version "1.10.0"
  revision 1
  depends_on "go" => :build
  depends_on "openssl@3"

  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    rebuild 2
    sha256 cellar: :any_skip_relocation, arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    sha256 cellar: :any_skip_relocation, x86_64_linux: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "hashicorp".to_string(),
            repo: "tap".to_string(),
            formula: "terraform".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert_eq!(formula.name, "terraform");
        assert_eq!(formula.versions.stable, "1.10.0");
        assert_eq!(formula.revision, 1);
        assert_eq!(formula.bottle.stable.rebuild, 2);
        assert_eq!(formula.dependencies, vec!["openssl@3".to_string()]);
        assert_eq!(formula.build_dependencies, vec!["go".to_string()]);
        assert!(formula.bottle.stable.files.contains_key("arm64_sonoma"));
        assert!(formula.bottle.stable.files.contains_key("x86_64_linux"));
    }

    #[test]
    fn defaults_to_ghcr_root_url_when_missing() {
        let source = r#"
class Terraform < Formula
  bottle do
    sha256 arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "hashicorp".to_string(),
            repo: "tap".to_string(),
            formula: "terraform".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        let url = &formula.bottle.stable.files["arm64_sonoma"].url;
        assert_eq!(
            url,
            "https://ghcr.io/v2/hashicorp/tap/terraform/blobs/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn builds_release_style_bottle_url() {
        let source = r#"
class Ttfb < Formula
  version "1.3.0"
  bottle do
    root_url "https://github.com/messense/homebrew-tap/releases/download/ttfb-1.3.0"
    sha256 x86_64_linux: "054859a821b01d3dd7236e71fbf106f7a694ded54ae6aaaed221b59d3b554c42"
  end
end
"#;
        let spec = TapFormulaRef {
            owner: "messense".to_string(),
            repo: "tap".to_string(),
            formula: "ttfb".to_string(),
        };
        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        let url = &formula.bottle.stable.files["x86_64_linux"].url;
        assert_eq!(
            url,
            "https://github.com/messense/homebrew-tap/releases/download/ttfb-1.3.0/ttfb-1.3.0.x86_64_linux.bottle.tar.gz"
        );
    }

    #[test]
    fn infers_version_from_url_when_version_field_missing() {
        let source = r#"
class Jaso < Formula
  url "https://github.com/cr0sh/jaso/archive/refs/tags/v1.0.1.tar.gz"
  bottle do
    root_url "https://github.com/simnalamburt/homebrew-x/releases/download/jaso-1.0.1"
    sha256 x86_64_linux: "76c0ea0751627a7aac5495c460eecd8a7823c86e5e55b078b5884056efa8ae7f"
  end
end
"#;
        let spec = TapFormulaRef {
            owner: "simnalamburt".to_string(),
            repo: "x".to_string(),
            formula: "jaso".to_string(),
        };
        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert_eq!(formula.versions.stable, "1.0.1");
        assert_eq!(
            formula.bottle.stable.files["x86_64_linux"].url,
            "https://github.com/simnalamburt/homebrew-x/releases/download/jaso-1.0.1/jaso-1.0.1.x86_64_linux.bottle.tar.gz"
        );
    }

    #[test]
    fn parses_bottle_block_with_nested_do_end_sections() {
        let source = r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    on_linux do
      sha256 x86_64_linux: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    end
    on_macos do
      sha256 arm64_sonoma: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    end
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "hashicorp".to_string(),
            repo: "tap".to_string(),
            formula: "terraform".to_string(),
        };
        let formula = parse_tap_formula_ruby(&spec, source).unwrap();

        assert!(formula.bottle.stable.files.contains_key("x86_64_linux"));
        assert!(formula.bottle.stable.files.contains_key("arm64_sonoma"));
    }

    #[test]
    fn parses_bottle_cellars() {
        let source = r#"
class Foo < Formula
  version "1.0.0"

  bottle do
    root_url "https://ghcr.io/v2/acme/tap"
    sha256 cellar: :any_skip_relocation, arm64_tahoe: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    sha256 cellar: :any,                 arm64_sequoia: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    sha256 cellar: "/usr/local/Cellar",  sonoma: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    sha256                               arm64_sonoma: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    sha256                               ventura: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    sha256                               x86_64_linux: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
  end
end
"#;
        let spec = TapFormulaRef {
            owner: "acme".to_string(),
            repo: "tap".to_string(),
            formula: "foo".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        let cellar = |tag: &str| formula.bottle.stable.files[tag].cellar.as_deref();

        assert_eq!(cellar("arm64_tahoe"), Some(":any_skip_relocation"));
        assert_eq!(cellar("arm64_sequoia"), Some(":any"));
        assert_eq!(cellar("sonoma"), Some("/usr/local/Cellar"));
        // No `cellar:` means pinned to the platform's default Cellar.
        assert_eq!(cellar("arm64_sonoma"), Some("/opt/homebrew/Cellar"));
        assert_eq!(cellar("ventura"), Some("/usr/local/Cellar"));
        assert_eq!(
            cellar("x86_64_linux"),
            Some("/home/linuxbrew/.linuxbrew/Cellar")
        );
    }

    #[test]
    fn supports_source_only_tap_formula_without_bottle_block() {
        let source = r#"
class OhMyPosh < Formula
  version "29.3.0"
  url "https://github.com/JanDeDobbeleer/oh-my-posh/archive/v29.3.0.tar.gz"
  sha256 "ff39f6ef2b4ca2d7d766f2802520b023986a5d6dbcd59fba685a9e5bacf41993"
  depends_on "go@1.26" => :build
end
"#;

        let spec = TapFormulaRef {
            owner: "jandedobbeleer".to_string(),
            repo: "oh-my-posh".to_string(),
            formula: "oh-my-posh".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert!(formula.bottle.stable.files.is_empty());
        assert_eq!(formula.build_dependencies, vec!["go@1.26".to_string()]);

        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");
        assert_eq!(
            stable.url,
            "https://github.com/JanDeDobbeleer/oh-my-posh/archive/v29.3.0.tar.gz"
        );
        assert_eq!(
            stable.checksum.as_deref(),
            Some("ff39f6ef2b4ca2d7d766f2802520b023986a5d6dbcd59fba685a9e5bacf41993")
        );
    }

    #[test]
    fn source_url_parsing_ignores_nested_resource_blocks() {
        let source = r#"
class Example < Formula
  url "https://example.com/example-1.0.0.tar.gz"
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

  resource "extra" do
    url "https://example.com/resource.tar.gz"
    sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "someone".to_string(),
            repo: "tap".to_string(),
            formula: "example".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");

        assert_eq!(stable.url, "https://example.com/example-1.0.0.tar.gz");
        assert_eq!(
            stable.checksum.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn source_url_without_sha256_is_unsupported() {
        let source = r#"
class Example < Formula
  url "https://example.com/example-1.0.0.tar.gz"
end
"#;

        let spec = TapFormulaRef {
            owner: "someone".to_string(),
            repo: "tap".to_string(),
            formula: "example".to_string(),
        };

        let err = parse_tap_formula_ruby(&spec, source).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedFormula { reason, .. }
            if reason.contains("missing sha256")
        ));
    }

    #[test]
    fn source_url_without_top_level_sha256_is_unsupported_even_if_nested_has_sha256() {
        let source = r#"
class Example < Formula
  url "https://example.com/example-1.0.0.tar.gz"

  resource "extra" do
    url "https://example.com/resource.tar.gz"
    sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "someone".to_string(),
            repo: "tap".to_string(),
            formula: "example".to_string(),
        };

        let err = parse_tap_formula_ruby(&spec, source).unwrap_err();
        assert!(matches!(
            err,
            Error::UnsupportedFormula { reason, .. }
            if reason.contains("missing sha256")
        ));
    }

    #[test]
    fn dependency_parsing_ignores_nested_blocks() {
        let source = r#"
class Example < Formula
  url "https://example.com/example-1.0.0.tar.gz"
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  depends_on "openssl@3"
  depends_on "go" => :build

  resource "extra" do
    depends_on "python@3.12"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "someone".to_string(),
            repo: "tap".to_string(),
            formula: "example".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert_eq!(formula.dependencies, vec!["openssl@3".to_string()]);
        assert_eq!(formula.build_dependencies, vec!["go".to_string()]);
    }

    #[test]
    fn parser_does_not_treat_do_inside_strings_as_block_start() {
        let source = r#"
class Example < Formula
  desc "A tool to do amazing things"
  url "https://example.com/example-1.0.0.tar.gz"
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  depends_on "openssl@3"
  depends_on "go" => :build

  resource "extra" do |r|
    depends_on "python@3.12"
    r.url "https://example.com/resource.tar.gz"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "someone".to_string(),
            repo: "tap".to_string(),
            formula: "example".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert_eq!(formula.dependencies, vec!["openssl@3".to_string()]);
        assert_eq!(formula.build_dependencies, vec!["go".to_string()]);

        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");
        assert_eq!(stable.url, "https://example.com/example-1.0.0.tar.gz");
        assert_eq!(
            stable.checksum.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn returns_unsupported_formula_when_neither_bottle_nor_source_is_available() {
        let source = r#"
class Terraform < Formula
  version "1.10.0"
end
"#;

        let spec = TapFormulaRef {
            owner: "hashicorp".to_string(),
            repo: "tap".to_string(),
            formula: "terraform".to_string(),
        };

        let err = parse_tap_formula_ruby(&spec, source).unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormula { .. }));
    }

    #[test]
    fn resolves_on_platform_blocks_in_tap_formula() {
        let source = r#"
class Sag < Formula
  version "0.2.2"

  on_macos do
    url "https://example.com/sag_darwin.tar.gz"
    sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end

  on_linux do
    url "https://example.com/sag_linux.tar.gz"
    sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "steipete".to_string(),
            repo: "tap".to_string(),
            formula: "sag".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert_eq!(formula.versions.stable, "0.2.2");

        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");

        #[cfg(target_os = "macos")]
        {
            assert_eq!(stable.url, "https://example.com/sag_darwin.tar.gz");
            assert_eq!(
                stable.checksum.as_deref(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            );
        }

        #[cfg(target_os = "linux")]
        {
            assert_eq!(stable.url, "https://example.com/sag_linux.tar.gz");
            assert_eq!(
                stable.checksum.as_deref(),
                Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            );
        }
    }

    #[test]
    fn resolves_nested_arch_conditional_in_on_platform_block() {
        #[cfg(target_os = "linux")]
        {
            let source = r#"
class Sag < Formula
  version "0.2.2"

  on_linux do
    if Hardware::CPU.arm?
      url "https://example.com/sag_arm.tar.gz"
      sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      depends_on "go" => :build
    else
      url "https://example.com/sag_x86.tar.gz"
      sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    end
  end
end
"#;

            let spec = TapFormulaRef {
                owner: "steipete".to_string(),
                repo: "tap".to_string(),
                formula: "sag".to_string(),
            };
            let formula = parse_tap_formula_ruby(&spec, source).unwrap();
            let stable = formula
                .urls
                .as_ref()
                .and_then(|u| u.stable.as_ref())
                .expect("stable source url should be parsed");

            #[cfg(target_arch = "aarch64")]
            {
                assert_eq!(stable.url, "https://example.com/sag_arm.tar.gz");
                assert_eq!(formula.build_dependencies, vec!["go".to_string()]);
            }

            #[cfg(target_arch = "x86_64")]
            {
                assert_eq!(stable.url, "https://example.com/sag_x86.tar.gz");
                assert!(formula.build_dependencies.is_empty());
            }
        }
    }

    #[test]
    fn resolves_version_interpolation_in_urls() {
        let source = r#"
class Sag < Formula
  version "0.2.2"
  url "https://example.com/sag/v#{version}/sag_#{version}_linux.tar.gz"
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
end
"#;

        let spec = TapFormulaRef {
            owner: "steipete".to_string(),
            repo: "tap".to_string(),
            formula: "sag".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");
        assert_eq!(
            stable.url,
            "https://example.com/sag/v0.2.2/sag_0.2.2_linux.tar.gz"
        );
    }

    #[test]
    fn resolves_real_world_sag_formula_with_platform_and_arch_conditionals() {
        // Closely mirrors the actual steipete/tap/sag formula structure
        let source = r#"
class Sag < Formula
  desc "Command-line ElevenLabs TTS with mac-style flags"
  homepage "https://github.com/steipete/sag"
  version "0.2.2"
  license "MIT"

  on_macos do
    url "https://github.com/steipete/sag/releases/download/v#{version}/sag_#{version}_darwin_universal.tar.gz"
    sha256 "0554baef912217d9e1f3988fb6d7492d46d2f49105a5eb9175e3f861f39cd289"
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/steipete/sag/archive/refs/tags/v#{version}.tar.gz"
      sha256 "82a09382d6803396e29a3fdc1d1d4982ffd97d69b5c6f9b6e31d0fb3818450db"
      depends_on "go" => :build
    else
      url "https://github.com/steipete/sag/releases/download/v#{version}/sag_#{version}_linux_amd64.tar.gz"
      sha256 "fddfe2553648fd9cd3446610c55176d897850d79f4aa95d60b605d387ffabdc8"
    end
  end

  def install
    if File.exist?("sag")
      bin.install "sag"
    else
      system "go", "build", "./cmd/sag"
    end
  end

  test do
    assert_match version.to_s, shell_output("sag --version")
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "steipete".to_string(),
            repo: "tap".to_string(),
            formula: "sag".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert_eq!(formula.name, "sag");
        assert_eq!(formula.versions.stable, "0.2.2");

        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");

        // Version interpolation should be resolved
        assert!(
            !stable.url.contains("#{version}"),
            "url should not contain unresolved interpolation: {}",
            stable.url
        );
        assert!(
            stable.url.contains("0.2.2"),
            "url should contain resolved version: {}",
            stable.url
        );

        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                stable.url,
                "https://github.com/steipete/sag/releases/download/v0.2.2/sag_0.2.2_darwin_universal.tar.gz"
            );
        }

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            assert_eq!(
                stable.url,
                "https://github.com/steipete/sag/releases/download/v0.2.2/sag_0.2.2_linux_amd64.tar.gz"
            );
            assert!(formula.build_dependencies.is_empty());
        }

        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            assert_eq!(
                stable.url,
                "https://github.com/steipete/sag/archive/refs/tags/v0.2.2.tar.gz"
            );
            assert_eq!(formula.build_dependencies, vec!["go".to_string()]);
        }
    }

    #[test]
    fn preprocessing_preserves_formulas_without_platform_blocks() {
        // Ensure formulas without any platform blocks are not affected
        let source = r#"
class OhMyPosh < Formula
  version "29.3.0"
  url "https://github.com/JanDeDobbeleer/oh-my-posh/archive/v29.3.0.tar.gz"
  sha256 "ff39f6ef2b4ca2d7d766f2802520b023986a5d6dbcd59fba685a9e5bacf41993"
  depends_on "go@1.26" => :build
end
"#;

        let spec = TapFormulaRef {
            owner: "jandedobbeleer".to_string(),
            repo: "oh-my-posh".to_string(),
            formula: "oh-my-posh".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert_eq!(formula.versions.stable, "29.3.0");
        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");
        assert_eq!(
            stable.url,
            "https://github.com/JanDeDobbeleer/oh-my-posh/archive/v29.3.0.tar.gz"
        );
        assert_eq!(formula.build_dependencies, vec!["go@1.26".to_string()]);
    }

    #[test]
    fn resolves_on_arm_and_on_intel_blocks() {
        let source = r#"
class Example < Formula
  version "1.0.0"
  on_arm do
    url "https://example.com/example_arm.tar.gz"
    sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
  on_intel do
    url "https://example.com/example_x86.tar.gz"
    sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "someone".to_string(),
            repo: "tap".to_string(),
            formula: "example".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        let stable = formula
            .urls
            .as_ref()
            .and_then(|u| u.stable.as_ref())
            .expect("stable source url should be parsed");

        #[cfg(target_arch = "aarch64")]
        assert_eq!(stable.url, "https://example.com/example_arm.tar.gz");

        #[cfg(target_arch = "x86_64")]
        assert_eq!(stable.url, "https://example.com/example_x86.tar.gz");
    }

    #[test]
    fn platform_deps_are_resolved_from_matching_block() {
        let source = r#"
class Example < Formula
  version "1.0.0"
  url "https://example.com/example.tar.gz"
  sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  depends_on "common-dep"

  on_macos do
    depends_on "macos-only-dep"
  end

  on_linux do
    depends_on "linux-only-dep"
  end
end
"#;

        let spec = TapFormulaRef {
            owner: "someone".to_string(),
            repo: "tap".to_string(),
            formula: "example".to_string(),
        };

        let formula = parse_tap_formula_ruby(&spec, source).unwrap();
        assert!(formula.dependencies.contains(&"common-dep".to_string()));

        #[cfg(target_os = "macos")]
        {
            assert!(formula.dependencies.contains(&"macos-only-dep".to_string()));
            assert!(!formula.dependencies.contains(&"linux-only-dep".to_string()));
        }

        #[cfg(target_os = "linux")]
        {
            assert!(formula.dependencies.contains(&"linux-only-dep".to_string()));
            assert!(!formula.dependencies.contains(&"macos-only-dep".to_string()));
        }
    }
}
