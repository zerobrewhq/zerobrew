use std::borrow::Cow;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::checksum::verify_sha256_bytes;
use crate::network::cache::{ApiCache, CacheEntry, IndexEntry, IndexMeta};
use crate::network::suggest::rank_formula_suggestions;
use crate::network::tap_formula::{parse_tap_formula_ref, parse_tap_formula_ruby};
use futures_util::stream::{self, StreamExt};
use serde_json::value::RawValue;
use tokio::sync::OnceCell;
use tracing::{debug, warn};
use zb_core::{Error, Formula};

/// How long the formula index is trusted before it is revalidated, unless
/// `ZEROBREW_API_AUTO_UPDATE_SECS` says otherwise. Matches the API's own
/// `max-age`.
const INDEX_MAX_AGE: Duration = Duration::from_secs(600);

/// The names in one entry of the bulk API file.
#[derive(serde::Deserialize)]
struct IndexNames<'a> {
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    aliases: Vec<Cow<'a, str>>,
    #[serde(borrow, default)]
    oldnames: Vec<Cow<'a, str>>,
}

/// Split the bulk API file into index entries without building a tree for
/// its 30 MB: each formula's JSON is kept verbatim.
fn index_entries(body: &str) -> Result<Vec<IndexEntry<'_>>, Error> {
    let raws: Vec<&RawValue> =
        serde_json::from_str(body).map_err(Error::network("failed to parse bulk formula JSON"))?;
    Ok(raws
        .into_iter()
        .filter_map(|raw| {
            let names: IndexNames = serde_json::from_str(raw.get()).ok()?;
            let name = names.name?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let aliases = names
                .aliases
                .iter()
                .chain(&names.oldnames)
                .map(|alias| alias.trim().to_string())
                .filter(|alias| !alias.is_empty() && *alias != name)
                .collect();
            Some(IndexEntry {
                name,
                body: raw.get(),
                aliases,
            })
        })
        .collect())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn index_max_age() -> Duration {
    std::env::var("ZEROBREW_API_AUTO_UPDATE_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(INDEX_MAX_AGE)
}

const HOMEBREW_CORE_RAW_BASE: &str =
    "https://raw.githubusercontent.com/Homebrew/homebrew-core/main";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RubySourceLocator<'a> {
    CoreRelativePath(&'a str),
    AbsoluteUrl(&'a str),
    TapEncodedUrl(&'a str),
}

impl<'a> RubySourceLocator<'a> {
    const TAP_URL_PREFIX: &'static str = "tap-rb-url:";

    fn parse(input: &'a str) -> Self {
        if let Some(encoded_url) = input.strip_prefix(Self::TAP_URL_PREFIX) {
            return Self::TapEncodedUrl(encoded_url);
        }

        if input.starts_with("https://") || input.starts_with("http://") {
            return Self::AbsoluteUrl(input);
        }

        Self::CoreRelativePath(input)
    }

    fn source_id(self, original: &'a str) -> &'a str {
        match self {
            Self::CoreRelativePath(_) => original,
            Self::AbsoluteUrl(url) => url,
            Self::TapEncodedUrl(url) => url,
        }
    }

    fn to_url(self) -> String {
        match self {
            Self::CoreRelativePath(path) => format!("{HOMEBREW_CORE_RAW_BASE}/{path}"),
            Self::AbsoluteUrl(url) | Self::TapEncodedUrl(url) => url.to_string(),
        }
    }

    fn encode_tap_url(url: &str) -> String {
        format!("{}{}", Self::TAP_URL_PREFIX, url)
    }
}

enum CachedGetResult {
    Cached(String),
    Fresh(reqwest::Response),
}

#[derive(Debug)]
pub struct ApiClient {
    base_url: String,
    cask_base_url: String,
    tap_raw_base_url: String,
    client: reqwest::Client,
    cache: Option<ApiCache>,
    /// Whether the index in `cache` is usable this run, decided by the first
    /// caller that needs it; see [`ApiClient::ensure_index`].
    index_usable: OnceCell<bool>,
    formula_candidates: RwLock<Option<Arc<[String]>>>,
}

impl ApiClient {
    const DEFAULT_BASE_URL: &'static str = "https://formulae.brew.sh/api/formula";

    pub fn new() -> Self {
        Self::build_client(Self::DEFAULT_BASE_URL.to_string())
    }

    /// Rejects non-http(s) schemes and URLs containing credentials.
    pub fn with_base_url(base_url: String) -> Result<Self, Error> {
        let parsed = reqwest::Url::parse(&base_url).map_err(|e| Error::InvalidArgument {
            message: format!("invalid API base URL: {e}"),
        })?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(Error::InvalidArgument {
                message: format!(
                    "API base URL must use http or https scheme, got: {}",
                    parsed.scheme()
                ),
            });
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(Error::InvalidArgument {
                message: "Bad ZEROBREW_API_URL configuration".to_string(),
            });
        }

        Ok(Self::build_client(base_url))
    }

    fn build_client(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("zerobrew/0.1")
            .pool_max_idle_per_host(20)
            .use_preconfigured_tls((*crate::network::tls::shared_tls_config()).clone())
            .build()
            .expect("failed to build HTTP client");

        Self {
            base_url,
            cask_base_url: "https://formulae.brew.sh/api/cask".to_string(),
            tap_raw_base_url: "https://raw.githubusercontent.com".to_string(),
            client,
            cache: None,
            index_usable: OnceCell::new(),
            formula_candidates: RwLock::new(None),
        }
    }

    #[cfg(test)]
    pub fn with_tap_raw_base_url(mut self, tap_raw_base_url: String) -> Self {
        self.tap_raw_base_url = tap_raw_base_url;
        self
    }

    #[cfg(test)]
    pub fn with_cask_base_url(mut self, cask_base_url: String) -> Self {
        self.cask_base_url = cask_base_url;
        self
    }

    pub fn with_cache(mut self, cache: ApiCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Clear all cached API responses. Returns the number removed.
    pub fn clear_cache(&self) -> Result<usize, Error> {
        match &self.cache {
            Some(cache) => cache
                .clear()
                .map_err(Error::store("failed to clear API cache")),
            None => Ok(0),
        }
    }

    pub async fn fetch_formula_rb(
        &self,
        ruby_source_path: &str,
        cache_dir: &std::path::Path,
        expected_sha256: Option<&str>,
    ) -> Result<std::path::PathBuf, Error> {
        let locator = RubySourceLocator::parse(ruby_source_path);
        let source_id = locator.source_id(ruby_source_path);
        let url = locator.to_url();

        self.fetch_formula_rb_from_url(source_id, &url, cache_dir, expected_sha256)
            .await
    }

    async fn fetch_formula_rb_from_url(
        &self,
        ruby_source_path: &str,
        url: &str,
        cache_dir: &std::path::Path,
        expected_sha256: Option<&str>,
    ) -> Result<std::path::PathBuf, Error> {
        let cache_key = format!("rb:{url}");
        if let Some(entry) = self.cache.as_ref().and_then(|c| c.get(&cache_key)) {
            verify_sha256_bytes(entry.body.as_bytes(), expected_sha256)
                .map_err(|e| Self::map_formula_rb_checksum_error(e, ruby_source_path, "cache"))?;

            let dest = cache_dir.join(ruby_source_path.replace('/', "_"));
            std::fs::create_dir_all(cache_dir)
                .map_err(Error::file("failed to create rb cache dir"))?;
            std::fs::write(&dest, entry.body.as_bytes())
                .map_err(Error::file("failed to write cached rb file"))?;
            return Ok(dest);
        }

        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(Error::network("failed to fetch formula rb"))?;

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("formula rb fetch returned HTTP {}", response.status()),
            });
        }

        let body = response
            .text()
            .await
            .map_err(Error::network("failed to read formula rb response"))?;

        verify_sha256_bytes(body.as_bytes(), expected_sha256)
            .map_err(|e| Self::map_formula_rb_checksum_error(e, ruby_source_path, "network"))?;

        if let Some(ref cache) = self.cache {
            let entry = CacheEntry {
                etag: None,
                last_modified: None,
                body: body.clone(),
            };
            let _ = cache.put(&cache_key, &entry);
        }

        let dest = cache_dir.join(ruby_source_path.replace('/', "_"));
        std::fs::create_dir_all(cache_dir).map_err(Error::file("failed to create rb cache dir"))?;
        std::fs::write(&dest, body.as_bytes()).map_err(Error::file("failed to write rb file"))?;

        Ok(dest)
    }

    fn map_formula_rb_checksum_error(err: Error, ruby_source_path: &str, source: &str) -> Error {
        match err {
            Error::ChecksumMismatch { .. } => err,
            Error::InvalidArgument { message } => Error::InvalidArgument {
                message: format!(
                    "invalid ruby_source_checksum for '{ruby_source_path}' (source: {source}): {message}"
                ),
            },
            other => other,
        }
    }

    async fn cached_get(&self, url: &str) -> Result<CachedGetResult, Error> {
        let cached_entry = self.cache.as_ref().and_then(|c| c.get(url));

        let mut request = self.client.get(url);

        if let Some(ref entry) = cached_entry {
            if let Some(ref etag) = entry.etag {
                request = request.header("If-None-Match", etag.as_str());
            }
            if let Some(ref last_modified) = entry.last_modified {
                request = request.header("If-Modified-Since", last_modified.as_str());
            }
        }

        let response = request.send().await.map_err(|e| Error::NetworkFailure {
            message: e.to_string(),
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(entry) = cached_entry
        {
            return Ok(CachedGetResult::Cached(entry.body));
        }

        Ok(CachedGetResult::Fresh(response))
    }

    fn store_response_in_cache(
        &self,
        url: &str,
        etag: Option<String>,
        last_modified: Option<String>,
        body: &str,
    ) {
        if let Some(ref cache) = self.cache {
            let entry = CacheEntry {
                etag,
                last_modified,
                body: body.to_string(),
            };
            let _ = cache.put(url, &entry);
        }
    }

    /// Look a formula up in the index, then by alias, then on the API. A
    /// name the index does not have may be newer than the index, so it is
    /// still asked for; a 404 there is final.
    pub async fn get_formula(&self, name: &str) -> Result<Formula, Error> {
        if let Some(spec) = parse_tap_formula_ref(name) {
            return self.get_tap_formula(&spec).await;
        }

        let parse_body = |body: String| {
            serde_json::from_str(&body).map_err(Error::network("failed to parse formula JSON"))
        };

        let mut lookup = name.to_string();
        if self.ensure_index().await
            && let Some(cache) = &self.cache
        {
            if let Some(body) = cache.index_formula(name) {
                return parse_body(body);
            }
            if let Some(canonical) = cache.index_alias(name) {
                if let Some(body) = cache.index_formula(&canonical) {
                    return parse_body(body);
                }
                lookup = canonical;
            }
        }

        match self.fetch_formula_json(&lookup).await {
            Ok(body) => parse_body(body),
            Err(Error::MissingFormula { .. }) => Err(Error::MissingFormula {
                name: name.to_string(),
            }),
            Err(e) => Err(e),
        }
    }

    async fn fetch_formula_json(&self, name: &str) -> Result<String, Error> {
        let url = format!("{}/{}.json", self.base_url, name);

        match self.cached_get(&url).await? {
            CachedGetResult::Cached(body) => Ok(body),
            CachedGetResult::Fresh(response) => {
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::MissingFormula {
                        name: name.to_string(),
                    });
                }
                if !response.status().is_success() {
                    return Err(Error::NetworkFailure {
                        message: format!("HTTP {}", response.status()),
                    });
                }

                let etag = response
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let last_modified = response
                    .headers()
                    .get("last-modified")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let body = response
                    .text()
                    .await
                    .map_err(Error::network("failed to read response body"))?;

                self.store_response_in_cache(&url, etag, last_modified, &body);
                Ok(body)
            }
        }
    }

    fn index_url(&self) -> String {
        format!("{}.json", self.base_url)
    }

    /// Whether the index can serve lookups this run. The first caller
    /// revalidates it when it is older than the max age; a failure there
    /// keeps whatever index exists, and without any index every lookup goes
    /// to the API as before.
    async fn ensure_index(&self) -> bool {
        *self
            .index_usable
            .get_or_init(|| async {
                let Some(cache) = &self.cache else {
                    return false;
                };
                match self.refresh_index(false).await {
                    Ok(()) => cache.index_meta().is_some(),
                    Err(e) => {
                        if cache.index_meta().is_some() {
                            warn!(error = %e, "could not refresh the formula index; using the cached one");
                            true
                        } else {
                            debug!(error = %e, "no formula index available; fetching formulas one by one");
                            false
                        }
                    }
                }
            })
            .await
    }

    /// Bring the index up to date: a conditional GET of the bulk API file
    /// when it is older than the max age, or always when `force` is set. A
    /// 304 only bumps the timestamp; a 200 replaces the whole index.
    pub async fn refresh_index(&self, force: bool) -> Result<(), Error> {
        let Some(cache) = &self.cache else {
            return Ok(());
        };
        let meta = cache.index_meta();
        if !force
            && let Some(meta) = &meta
            && unix_now().saturating_sub(meta.fetched_at) < index_max_age().as_secs() as i64
        {
            return Ok(());
        }

        let url = self.index_url();
        let mut request = self.client.get(&url);
        if let Some(meta) = &meta {
            if let Some(etag) = &meta.etag {
                request = request.header("If-None-Match", etag.as_str());
            }
            if let Some(last_modified) = &meta.last_modified {
                request = request.header("If-Modified-Since", last_modified.as_str());
            }
        }
        let response = request.send().await.map_err(|e| Error::NetworkFailure {
            message: e.to_string(),
        })?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED && meta.is_some() {
            cache
                .touch_index(unix_now())
                .map_err(Error::store("failed to update the formula index"))?;
            debug!("formula index is current");
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("bulk formula fetch returned HTTP {}", response.status()),
            });
        }

        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let new_meta = IndexMeta {
            etag: header("etag"),
            last_modified: header("last-modified"),
            fetched_at: unix_now(),
        };
        let body = response
            .text()
            .await
            .map_err(Error::network("failed to read bulk formula response body"))?;
        let count = cache
            .replace_index(index_entries(&body)?, &new_meta)
            .map_err(Error::store("failed to store the formula index"))?;
        debug!(count, "formula index refreshed");
        Ok(())
    }

    /// How many formulas the index holds.
    pub fn index_len(&self) -> usize {
        self.cache
            .as_ref()
            .and_then(|cache| cache.index_formula_count().ok())
            .unwrap_or(0)
    }

    pub async fn suggest_formulas(&self, query: &str, limit: usize) -> Result<Vec<String>, Error> {
        if limit == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }

        if parse_tap_formula_ref(query).is_some() || query.starts_with("cask:") {
            return Ok(Vec::new());
        }

        let candidates = self.formula_candidates().await?;
        Ok(rank_formula_suggestions(query, &candidates, limit))
    }

    /// Every formula name, alias and old name: from the index, or straight
    /// from the bulk API file when there is no cache to index into.
    async fn formula_candidates(&self) -> Result<Arc<[String]>, Error> {
        if let Some(candidates) = self.formula_candidates.read().ok().and_then(|c| c.clone()) {
            return Ok(candidates);
        }

        let names: Vec<String> = match &self.cache {
            Some(cache) if self.ensure_index().await => cache
                .index_names()
                .map_err(Error::store("failed to read the formula index"))?,
            _ => {
                let response = self
                    .client
                    .get(self.index_url())
                    .send()
                    .await
                    .map_err(|e| Error::NetworkFailure {
                        message: e.to_string(),
                    })?;
                if !response.status().is_success() {
                    return Err(Error::NetworkFailure {
                        message: format!("bulk formula fetch returned HTTP {}", response.status()),
                    });
                }
                let body = response
                    .text()
                    .await
                    .map_err(Error::network("failed to read bulk formula response body"))?;
                index_entries(&body)?
                    .into_iter()
                    .flat_map(|entry| std::iter::once(entry.name).chain(entry.aliases))
                    .collect()
            }
        };
        let candidates: Arc<[String]> = names.into();
        if let Ok(mut cached) = self.formula_candidates.write() {
            *cached = Some(Arc::clone(&candidates));
        }
        Ok(candidates)
    }

    pub async fn get_cask(&self, token: &str) -> Result<serde_json::Value, Error> {
        let url = format!("{}/{}.json", self.cask_base_url, token);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::NetworkFailure {
                message: e.to_string(),
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::MissingFormula {
                name: format!("cask:{token}"),
            });
        }

        if !response.status().is_success() {
            return Err(Error::NetworkFailure {
                message: format!("HTTP {}", response.status()),
            });
        }

        response
            .json::<serde_json::Value>()
            .await
            .map_err(Error::network("failed to parse cask JSON"))
    }

    async fn get_tap_formula(
        &self,
        spec: &crate::network::tap_formula::TapFormulaRef,
    ) -> Result<Formula, Error> {
        let candidate_repos = if spec.repo.starts_with("homebrew-") {
            vec![
                spec.repo.clone(),
                spec.repo.trim_start_matches("homebrew-").to_string(),
            ]
        } else {
            vec![format!("homebrew-{}", spec.repo), spec.repo.clone()]
        };
        let first_char = spec.formula.chars().next().unwrap_or('x');
        let candidate_paths = [
            format!("Formula/{}.rb", spec.formula),
            format!("Formula/{first_char}/{}.rb", spec.formula),
            format!("HomebrewFormula/{}.rb", spec.formula),
            format!("HomebrewFormula/{first_char}/{}.rb", spec.formula),
            format!("{}.rb", spec.formula),
        ];
        let branches = ["main", "master"];

        let mut last_status: Option<reqwest::StatusCode> = None;
        let mut last_network_error: Option<Error> = None;
        let mut saw_non_404_status = false;

        for repo in candidate_repos {
            for branch in branches {
                let base_prefix = format!(
                    "{}/{}/{}/{}/",
                    self.tap_raw_base_url.trim_end_matches('/'),
                    spec.owner,
                    repo,
                    branch,
                );
                let client = self.client.clone();
                let mut responses = stream::iter(candidate_paths.iter().map(|candidate_path| {
                    let client = client.clone();
                    let url = format!("{base_prefix}{candidate_path}");
                    async move { (url.clone(), client.get(&url).send().await) }
                }))
                .buffered(2);

                while let Some((url, response)) = responses.next().await {
                    match response {
                        Ok(response) => {
                            let status = response.status();
                            if status.is_success() {
                                let body = response
                                    .text()
                                    .await
                                    .map_err(Error::network("failed to read tap formula body"))?;
                                let mut formula = parse_tap_formula_ruby(spec, &body)?;
                                formula.ruby_source_path =
                                    Some(RubySourceLocator::encode_tap_url(&url));
                                return Ok(formula);
                            }

                            if status != reqwest::StatusCode::NOT_FOUND {
                                saw_non_404_status = true;
                            }
                            last_status = Some(status);
                        }
                        Err(e) => {
                            last_network_error = Some(Error::NetworkFailure {
                                message: e.to_string(),
                            });
                        }
                    }
                }
            }
        }

        if !saw_non_404_status
            && last_network_error.is_none()
            && last_status == Some(reqwest::StatusCode::NOT_FOUND)
        {
            return Err(Error::MissingFormula {
                name: format!("{}/{}/{}", spec.owner, spec.repo, spec.formula),
            });
        }

        if let Some(err) = last_network_error {
            return Err(err);
        }

        Err(Error::NetworkFailure {
            message: format!(
                "failed to fetch tap formula '{}/{}/{}' (last status: {})",
                spec.owner,
                spec.repo,
                spec.formula,
                last_status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
        })
    }
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn with_base_url_rejects_non_http_schemes() {
        let err = ApiClient::with_base_url("ftp://example.com/api".into()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
        assert!(err.to_string().contains("http or https"));
    }

    #[test]
    fn with_base_url_rejects_embedded_credentials() {
        let err = ApiClient::with_base_url("https://user:pass@example.com/api".into()).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
    }

    #[test]
    fn with_base_url_rejects_garbage() {
        assert!(ApiClient::with_base_url("not a url".into()).is_err());
    }

    #[test]
    fn with_base_url_accepts_valid_https() {
        assert!(ApiClient::with_base_url("https://mirror.example.com/api/formula".into()).is_ok());
    }

    #[test]
    fn with_base_url_accepts_valid_http() {
        assert!(ApiClient::with_base_url("http://localhost:8080/api".into()).is_ok());
    }

    #[test]
    fn ruby_source_locator_parses_all_supported_kinds() {
        assert_eq!(
            RubySourceLocator::parse("Formula/f/foo.rb"),
            RubySourceLocator::CoreRelativePath("Formula/f/foo.rb")
        );
        assert_eq!(
            RubySourceLocator::parse("https://example.com/foo.rb"),
            RubySourceLocator::AbsoluteUrl("https://example.com/foo.rb")
        );
        let encoded = format!(
            "{}{}",
            RubySourceLocator::TAP_URL_PREFIX,
            "https://example.com/tap/foo.rb"
        );
        assert_eq!(
            RubySourceLocator::parse(&encoded),
            RubySourceLocator::TapEncodedUrl("https://example.com/tap/foo.rb")
        );
    }

    #[test]
    fn ruby_source_locator_resolves_urls_exhaustively() {
        assert_eq!(
            RubySourceLocator::CoreRelativePath("Formula/f/foo.rb").to_url(),
            "https://raw.githubusercontent.com/Homebrew/homebrew-core/main/Formula/f/foo.rb"
        );
        assert_eq!(
            RubySourceLocator::AbsoluteUrl("https://example.com/foo.rb").to_url(),
            "https://example.com/foo.rb"
        );
        assert_eq!(
            RubySourceLocator::TapEncodedUrl(
                "https://raw.githubusercontent.com/org/tap/main/foo.rb"
            )
            .to_url(),
            "https://raw.githubusercontent.com/org/tap/main/foo.rb"
        );
    }

    #[tokio::test]
    async fn fetches_formula_from_mock_server() {
        let mock_server = MockServer::start().await;

        let fixture = include_str!("../../../zb_core/fixtures/formula_foo.json");

        Mock::given(method("GET"))
            .and(path("/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri()).unwrap();
        let formula = client.get_formula("foo").await.unwrap();

        assert_eq!(formula.name, "foo");
        assert_eq!(formula.versions.stable, "1.2.3");
    }

    #[tokio::test]
    async fn returns_missing_formula_on_404() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/nonexistent.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri()).unwrap();
        let err = client.get_formula("nonexistent").await.unwrap_err();

        assert!(matches!(
            err,
            Error::MissingFormula { name } if name == "nonexistent"
        ));
    }

    #[tokio::test]
    async fn fetches_formula_from_tap_ruby_source() {
        let mock_server = MockServer::start().await;
        let rb = r#"
class Terraform < Formula
  version "1.10.0"
  depends_on "go"
  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    sha256 arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#;

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let formula = client.get_formula("hashicorp/tap/terraform").await.unwrap();

        assert_eq!(formula.name, "terraform");
        assert_eq!(formula.versions.stable, "1.10.0");
        assert!(formula.dependencies.contains(&"go".to_string()));
        assert!(formula.bottle.stable.files.contains_key("arm64_sonoma"));
        let expected_path = format!(
            "{}{}/hashicorp/homebrew-tap/main/Formula/terraform.rb",
            RubySourceLocator::TAP_URL_PREFIX,
            mock_server.uri()
        );
        assert_eq!(
            formula.ruby_source_path.as_deref(),
            Some(expected_path.as_str())
        );
    }

    #[tokio::test]
    async fn supports_source_only_tap_formula_without_bottle_block() {
        let mock_server = MockServer::start().await;
        let rb = r#"
class OhMyPosh < Formula
  version "29.3.0"
  url "https://github.com/JanDeDobbeleer/oh-my-posh/archive/v29.3.0.tar.gz"
  sha256 "ff39f6ef2b4ca2d7d766f2802520b023986a5d6dbcd59fba685a9e5bacf41993"
  depends_on "go@1.26" => :build
end
"#;

        Mock::given(method("GET"))
            .and(path(
                "/jandedobbeleer/homebrew-oh-my-posh/main/oh-my-posh.rb",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let formula = client
            .get_formula("jandedobbeleer/oh-my-posh/oh-my-posh")
            .await
            .unwrap();

        assert_eq!(formula.name, "oh-my-posh");
        assert!(formula.bottle.stable.files.is_empty());
        assert_eq!(formula.build_dependencies, vec!["go@1.26".to_string()]);
        assert!(formula.has_source_url());
        assert!(
            formula
                .ruby_source_path
                .as_deref()
                .is_some_and(|path| path.starts_with(RubySourceLocator::TAP_URL_PREFIX))
        );
    }

    #[tokio::test]
    async fn falls_back_to_master_when_main_missing_for_tap_formula() {
        let mock_server = MockServer::start().await;
        let rb = r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    sha256 arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#;

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/master/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let formula = client.get_formula("hashicorp/tap/terraform").await.unwrap();

        assert_eq!(formula.name, "terraform");
        assert_eq!(formula.versions.stable, "1.10.0");
    }

    #[tokio::test]
    async fn resolves_tap_formula_from_letter_subdirectory_path() {
        let mock_server = MockServer::start().await;
        let rb = r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    sha256 arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#;

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/t/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let formula = client.get_formula("hashicorp/tap/terraform").await.unwrap();

        assert_eq!(formula.name, "terraform");
        assert_eq!(formula.versions.stable, "1.10.0");
    }

    #[tokio::test]
    async fn resolves_tap_formula_from_homebrewformula_directory() {
        let mock_server = MockServer::start().await;
        let rb = r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    sha256 arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#;

        Mock::given(method("GET"))
            .and(path(
                "/hashicorp/homebrew-tap/main/HomebrewFormula/terraform.rb",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let formula = client.get_formula("hashicorp/tap/terraform").await.unwrap();

        assert_eq!(formula.name, "terraform");
        assert_eq!(formula.versions.stable, "1.10.0");
    }

    #[tokio::test]
    async fn resolves_tap_formula_from_homebrewformula_letter_subdirectory_path() {
        let mock_server = MockServer::start().await;
        let rb = r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    sha256 arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#;

        Mock::given(method("GET"))
            .and(path(
                "/hashicorp/homebrew-tap/main/HomebrewFormula/t/terraform.rb",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let formula = client.get_formula("hashicorp/tap/terraform").await.unwrap();

        assert_eq!(formula.name, "terraform");
        assert_eq!(formula.versions.stable, "1.10.0");
    }

    #[tokio::test]
    async fn resolves_tap_formula_from_repository_root() {
        let mock_server = MockServer::start().await;
        let rb = r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "https://ghcr.io/v2/hashicorp/tap"
    sha256 arm64_sonoma: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#;

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(rb))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let formula = client.get_formula("hashicorp/tap/terraform").await.unwrap();

        assert_eq!(formula.name, "terraform");
        assert_eq!(formula.versions.stable, "1.10.0");
    }

    #[tokio::test]
    async fn returns_missing_formula_when_all_tap_candidates_are_404() {
        let mock_server = MockServer::start().await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let err = client
            .get_formula("hashicorp/tap/terraform")
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::MissingFormula { name } if name == "hashicorp/tap/terraform"
        ));
    }

    #[tokio::test]
    async fn does_not_return_missing_formula_when_a_non_404_tap_status_is_seen() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let err = client
            .get_formula("hashicorp/tap/terraform")
            .await
            .unwrap_err();

        assert!(matches!(err, Error::NetworkFailure { .. }));
    }

    #[tokio::test]
    async fn fetch_formula_rb_supports_absolute_url_paths() {
        let mock_server = MockServer::start().await;
        let ruby_body = "class Foo < Formula\nend\n";

        Mock::given(method("GET"))
            .and(path("/custom/foo.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ruby_body))
            .mount(&mock_server)
            .await;

        let cache_dir = tempdir().unwrap();
        let client = ApiClient::new();

        let fetched = client
            .fetch_formula_rb(
                &format!("{}/custom/foo.rb", mock_server.uri()),
                cache_dir.path(),
                None,
            )
            .await
            .unwrap();

        assert!(fetched.exists());
    }

    #[tokio::test]
    async fn fetch_formula_rb_from_network_rejects_checksum_mismatch() {
        let mock_server = MockServer::start().await;
        let ruby_body = "class Foo < Formula\nend\n";

        Mock::given(method("GET"))
            .and(path("/Formula/f/foo.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ruby_body))
            .mount(&mock_server)
            .await;

        let cache_dir = tempdir().unwrap();
        let client = ApiClient::new();

        let err = client
            .fetch_formula_rb_from_url(
                "Formula/f/foo.rb",
                &format!("{}/Formula/f/foo.rb", mock_server.uri()),
                cache_dir.path(),
                Some(&"0".repeat(64)),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::ChecksumMismatch { .. }));
    }

    #[tokio::test]
    async fn fetch_formula_rb_from_cache_rejects_checksum_mismatch() {
        let cache = ApiCache::in_memory().unwrap();
        let cache_url = "https://example.invalid/Formula/f/foo.rb";
        cache
            .put(
                &format!("rb:{cache_url}"),
                &CacheEntry {
                    etag: None,
                    last_modified: None,
                    body: "class Foo < Formula\nend\n".to_string(),
                },
            )
            .unwrap();

        let cache_dir = tempdir().unwrap();
        let client = ApiClient::new().with_cache(cache);

        let err = client
            .fetch_formula_rb_from_url(
                "Formula/f/foo.rb",
                cache_url,
                cache_dir.path(),
                Some(&"f".repeat(64)),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, Error::ChecksumMismatch { .. }));
    }

    #[tokio::test]
    async fn fetches_cask_json() {
        let mock_server = MockServer::start().await;
        let cask_json = r#"{
  "token": "iterm2",
  "version": "3.5.0",
  "url": "https://example.com/iterm2.zip",
  "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "artifacts": [{"app":["iTerm.app"]}]
}"#;

        Mock::given(method("GET"))
            .and(path("/iterm2.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(cask_json))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(mock_server.uri())
            .unwrap()
            .with_cask_base_url(mock_server.uri());
        let cask = client.get_cask("iterm2").await.unwrap();
        assert_eq!(cask["token"], "iterm2");
        assert_eq!(cask["version"], "3.5.0");
    }

    #[tokio::test]
    async fn suggest_formulas_returns_ranked_matches_from_bulk_index() {
        let mock_server = MockServer::start().await;
        let bulk = r#"[
            {"name":"python","aliases":["python@3.13"],"oldnames":["python3"]},
            {"name":"pytest"},
            {"name":"pypy"}
        ]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let suggestions = client.suggest_formulas("pythn", 3).await.unwrap();

        assert_eq!(suggestions.first().map(String::as_str), Some("python"));
    }

    #[tokio::test]
    async fn suggest_formulas_reuses_cached_candidates_across_calls() {
        let mock_server = MockServer::start().await;
        let bulk = r#"[
            {"name":"python"},
            {"name":"pytest"},
            {"name":"pypy"}
        ]"#;

        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();

        let first = client.suggest_formulas("pythn", 3).await.unwrap();
        let second = client.suggest_formulas("pythn", 3).await.unwrap();

        assert_eq!(first.first().map(String::as_str), Some("python"));
        assert_eq!(second.first().map(String::as_str), Some("python"));
    }

    #[tokio::test]
    async fn suggest_formulas_returns_empty_for_tap_references() {
        let client = ApiClient::new();
        let suggestions = client
            .suggest_formulas("hashicorp/tap/terraform", 3)
            .await
            .unwrap();

        assert!(suggestions.is_empty());
    }

    fn bulk(entries: &[&str]) -> String {
        format!("[{}]", entries.join(","))
    }

    const FOO: &str = include_str!("../../../zb_core/fixtures/formula_foo.json");

    /// A client whose cache is `cache`, pointed at `server`'s `/formula`.
    fn indexed_client(server: &MockServer, cache: ApiCache) -> ApiClient {
        ApiClient::with_base_url(format!("{}/formula", server.uri()))
            .unwrap()
            .with_cache(cache)
    }

    #[tokio::test]
    async fn formulas_are_served_from_the_index_without_a_request_each() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(bulk(&[FOO, r#"{"name":"bar","versions":{"stable":"2"},"dependencies":[],"bottle":{"stable":{"files":{}}}}"#]))
                    .insert_header("etag", "\"abc123\""),
            )
            .expect(1)
            .mount(&mock_server)
            .await;
        // No per-formula endpoint at all: lookups must come from the index.

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        let foo = client.get_formula("foo").await.unwrap();
        let bar = client.get_formula("bar").await.unwrap();

        assert_eq!(foo.versions.stable, "1.2.3");
        assert_eq!(bar.versions.stable, "2");
        assert_eq!(client.index_len(), 2);
        let meta = client.cache.as_ref().unwrap().index_meta().unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"abc123\""));
    }

    #[tokio::test]
    async fn a_fresh_index_is_not_revalidated() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                index_entries(&bulk(&[FOO])).unwrap(),
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: unix_now(),
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");
    }

    #[tokio::test]
    async fn a_stale_index_is_revalidated_and_kept_on_304() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .and(header("If-None-Match", "\"abc123\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        let stale = unix_now() - 24 * 3600;
        cache
            .replace_index(
                index_entries(&bulk(&[FOO])).unwrap(),
                &IndexMeta {
                    etag: Some("\"abc123\"".into()),
                    last_modified: None,
                    fetched_at: stale,
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");
        let meta = client.cache.as_ref().unwrap().index_meta().unwrap();
        assert!(meta.fetched_at > stale, "304 must bump the timestamp");
        assert_eq!(meta.etag.as_deref(), Some("\"abc123\""));
    }

    #[tokio::test]
    async fn a_stale_index_is_replaced_on_200() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[FOO])))
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                [IndexEntry {
                    name: "gone".into(),
                    body: r#"{"name":"gone"}"#,
                    aliases: vec![],
                }],
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: 0,
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");
        assert!(
            client
                .cache
                .as_ref()
                .unwrap()
                .index_formula("gone")
                .is_none()
        );
    }

    #[tokio::test]
    async fn aliases_resolve_through_the_index() {
        let mock_server = MockServer::start().await;
        let foo_with_alias = FOO.replacen(
            "{",
            r#"{"aliases":["foo-alias"],"oldnames":["old-foo"],"#,
            1,
        );
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[&foo_with_alias])))
            .mount(&mock_server)
            .await;

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(client.get_formula("foo-alias").await.unwrap().name, "foo");
        assert_eq!(client.get_formula("old-foo").await.unwrap().name, "foo");
    }

    #[tokio::test]
    async fn a_name_missing_from_the_index_is_asked_for_once() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[FOO])))
            .mount(&mock_server)
            .await;
        // Newer than the index: the API still has it.
        Mock::given(method("GET"))
            .and(path("/formula/brand-new.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(FOO.replace("\"foo\"", "\"brand-new\"")),
            )
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/nonexistent.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(
            client.get_formula("brand-new").await.unwrap().name,
            "brand-new"
        );
        let err = client.get_formula("nonexistent").await.unwrap_err();
        assert!(matches!(err, Error::MissingFormula { name } if name == "nonexistent"));
    }

    #[tokio::test]
    async fn an_unreachable_index_falls_back_to_the_cached_one_or_the_api() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOO))
            .expect(1)
            .mount(&mock_server)
            .await;

        // No index yet: per-formula requests as before.
        let client = indexed_client(&mock_server, ApiCache::in_memory().unwrap());
        assert_eq!(client.get_formula("foo").await.unwrap().name, "foo");

        // A stale index that cannot be refreshed is still used.
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                index_entries(&bulk(&[FOO.replace("\"foo\"", "\"cached\"").as_str()])).unwrap(),
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: 0,
                },
            )
            .unwrap();
        let client = indexed_client(&mock_server, cache);
        assert_eq!(client.get_formula("cached").await.unwrap().name, "cached");
    }

    #[tokio::test]
    async fn refresh_index_forced_fetches_even_when_fresh() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/formula.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bulk(&[FOO])))
            .expect(1)
            .mount(&mock_server)
            .await;
        let cache = ApiCache::in_memory().unwrap();
        cache
            .replace_index(
                [],
                &IndexMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at: unix_now(),
                },
            )
            .unwrap();

        let client = indexed_client(&mock_server, cache);
        client.refresh_index(false).await.unwrap();
        assert_eq!(client.index_len(), 0);
        client.refresh_index(true).await.unwrap();
        assert_eq!(client.index_len(), 1);
    }

    #[test]
    fn index_entries_keep_bodies_verbatim_and_skip_nameless_entries() {
        let entries = index_entries(
            r#"[{"name":"python","aliases":["python@3.13"],"oldnames":["python3"],"x":1},{"aliases":["orphan"]},{"name":" ripgrep ","aliases":["rg","ripgrep"]}]"#,
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "python");
        assert_eq!(
            entries[0].body,
            r#"{"name":"python","aliases":["python@3.13"],"oldnames":["python3"],"x":1}"#
        );
        assert_eq!(entries[0].aliases, ["python@3.13", "python3"]);
        assert_eq!(entries[1].name, "ripgrep");
        assert_eq!(entries[1].aliases, ["rg"], "a name is not its own alias");
        assert!(index_entries("{}").is_err());
    }
}
