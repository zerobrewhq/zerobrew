//! Bearer tokens for container registries.
//!
//! Homebrew bottles live on GHCR, which answers anonymous requests with a
//! 401 carrying a `WWW-Authenticate` challenge. One token request covers any
//! number of `scope=` parameters, so a whole install plan can be authorised
//! up front and every bottle fetched with a single GET. The challenge flow
//! stays as the fallback for registries that were not pre-authorised.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderValue, WWW_AUTHENTICATE};
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use zb_core::Error;

use super::DownloadError;

/// How long a token is trusted when the registry does not say.
const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(240);

pub(crate) fn bearer_header(token: &str) -> Result<HeaderValue, Error> {
    HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::NetworkFailure {
        message: "auth token contains invalid header characters".into(),
    })
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub(crate) struct CachedToken {
    pub(crate) token: String,
    pub(crate) expires_at: Instant,
}

/// Tokens by the scope they were issued for.
pub(crate) type TokenCache = Arc<RwLock<HashMap<String, CachedToken>>>;

/// Where a registry hands out tokens, as its `WWW-Authenticate` header
/// describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenEndpoint {
    pub(crate) realm: String,
    pub(crate) service: String,
}

impl TokenEndpoint {
    /// The endpoints of registries we know without asking. Every Homebrew
    /// bottle comes from GHCR.
    pub(crate) fn known() -> HashMap<String, Self> {
        HashMap::from([(
            "ghcr.io".to_string(),
            Self {
                realm: "https://ghcr.io/token".to_string(),
                service: "ghcr.io".to_string(),
            },
        )])
    }

    /// Parse a `Bearer realm="...",service="...",scope="..."` challenge.
    fn from_challenge(header: &str) -> Result<(Self, String), Error> {
        let header = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| Error::NetworkFailure {
                message: "unsupported auth scheme".to_string(),
            })?;

        let mut realm = None;
        let mut service = None;
        let mut scope = None;

        for part in header.split(',') {
            let part = part.trim();
            if let Some((key, value)) = part.split_once('=') {
                let value = value.trim_matches('"');
                match key {
                    "realm" => realm = Some(value.to_string()),
                    "service" => service = Some(value.to_string()),
                    "scope" => scope = Some(value.to_string()),
                    _ => {}
                }
            }
        }

        let missing = |what: &str| Error::NetworkFailure {
            message: format!("missing {what} in WWW-Authenticate"),
        };
        Ok((
            Self {
                realm: realm.ok_or_else(|| missing("realm"))?,
                service: service.ok_or_else(|| missing("service"))?,
            },
            scope.ok_or_else(|| missing("scope"))?,
        ))
    }
}

/// The registry a blob URL points at and the pull scope it needs:
/// `https://<host>/v2/<owner>/<repo>/<name>/blobs/<digest>` needs
/// `repository:<owner>/<repo>/<name>:pull`. `None` for anything else.
pub(crate) fn registry_scope(url: &str) -> Option<(String, String)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = match (parsed.host_str()?, parsed.port()) {
        (host, Some(port)) => format!("{host}:{port}"),
        (host, None) => host.to_string(),
    };
    let mut segments = parsed.path_segments()?;
    if segments.next()? != "v2" {
        return None;
    }
    let owner = segments.next()?;
    let repo = segments.next()?;
    let name = segments.next()?;
    if segments.next()? != "blobs" || owner.is_empty() || repo.is_empty() || name.is_empty() {
        return None;
    }
    Some((host, format!("repository:{owner}/{repo}/{name}:pull")))
}

async fn cached_token(token_cache: &TokenCache, scope: &str) -> Option<String> {
    let cache = token_cache.read().await;
    cache
        .get(scope)
        .filter(|cached| cached.expires_at > Instant::now())
        .map(|cached| cached.token.clone())
}

pub(crate) async fn get_cached_token_for_url_internal(
    token_cache: &TokenCache,
    url: &str,
) -> Option<String> {
    let (_, scope) = registry_scope(url)?;
    cached_token(token_cache, &scope).await
}

/// GET `url`, with the cached token when there is one, answering a 401
/// challenge once. The response has a success status.
pub(crate) async fn fetch_download_response_internal(
    client: &reqwest::Client,
    token_cache: &TokenCache,
    url: &str,
) -> Result<reqwest::Response, DownloadError> {
    let cached_token = get_cached_token_for_url_internal(token_cache, url).await;

    let mut request = client.get(url);
    if let Some(token) = &cached_token {
        request = request.header(
            AUTHORIZATION,
            bearer_header(token).map_err(DownloadError::permanent)?,
        );
    }

    let response = request.send().await.map_err(|e| {
        DownloadError::transient(Error::NetworkFailure {
            message: e.to_string(),
        })
    })?;

    let response = if response.status() == StatusCode::UNAUTHORIZED {
        handle_auth_challenge_internal(client, token_cache, url, response).await?
    } else {
        response
    };

    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let error = Error::NetworkFailure {
        message: format!("HTTP {status}"),
    };
    // The registry or CDN is having a moment; a client error is final.
    if status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
    {
        Err(DownloadError::transient(error))
    } else {
        Err(DownloadError::permanent(error))
    }
}

async fn handle_auth_challenge_internal(
    client: &reqwest::Client,
    token_cache: &TokenCache,
    url: &str,
    response: reqwest::Response,
) -> Result<reqwest::Response, DownloadError> {
    let www_auth = match response.headers().get(WWW_AUTHENTICATE) {
        Some(value) => value
            .to_str()
            .map_err(|_| {
                DownloadError::permanent(Error::NetworkFailure {
                    message: "WWW-Authenticate header contains invalid characters".to_string(),
                })
            })?
            .to_string(),
        None => {
            return Err(DownloadError::transient(Error::NetworkFailure {
                message:
                    "server returned 401 without WWW-Authenticate header (may be rate limited)"
                        .to_string(),
            }));
        }
    };

    let token = fetch_bearer_token_internal(client, token_cache, &www_auth).await?;

    let response = client
        .get(url)
        .header(
            AUTHORIZATION,
            bearer_header(&token).map_err(DownloadError::permanent)?,
        )
        .send()
        .await
        .map_err(|e| {
            DownloadError::transient(Error::NetworkFailure {
                message: e.to_string(),
            })
        })?;

    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(DownloadError::permanent(Error::NetworkFailure {
            message: "authentication failed: token was rejected by server".to_string(),
        }));
    }

    Ok(response)
}

/// Answer a challenge: use the cached token for its scope, or fetch one.
async fn fetch_bearer_token_internal(
    client: &reqwest::Client,
    token_cache: &TokenCache,
    www_authenticate: &str,
) -> Result<String, DownloadError> {
    let (endpoint, scope) =
        TokenEndpoint::from_challenge(www_authenticate).map_err(DownloadError::permanent)?;

    if let Some(token) = cached_token(token_cache, &scope).await {
        return Ok(token);
    }

    request_token(client, token_cache, &endpoint, &[scope]).await
}

/// Fetch one token for `scopes` and cache it under each of them.
async fn request_token(
    client: &reqwest::Client,
    token_cache: &TokenCache,
    endpoint: &TokenEndpoint,
    scopes: &[String],
) -> Result<String, DownloadError> {
    let params: Vec<(&str, &str)> = std::iter::once(("service", endpoint.service.as_str()))
        .chain(scopes.iter().map(|scope| ("scope", scope.as_str())))
        .collect();
    let token_url = reqwest::Url::parse_with_params(&endpoint.realm, &params).map_err(|e| {
        DownloadError::permanent(Error::network("failed to construct token URL")(e))
    })?;

    let response = client
        .get(token_url)
        .send()
        .await
        .map_err(|e| DownloadError::transient(Error::network("token request failed")(e)))?;

    if !response.status().is_success() {
        return Err(DownloadError::permanent(Error::NetworkFailure {
            message: format!("token request returned HTTP {}", response.status()),
        }));
    }

    let token_response: TokenResponse = response.json().await.map_err(|e| {
        DownloadError::permanent(Error::network("failed to parse token response")(e))
    })?;

    let ttl = token_response
        .expires_in
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TOKEN_TTL);
    let expires_at = Instant::now() + ttl;
    {
        let mut cache = token_cache.write().await;
        for scope in scopes {
            cache.insert(
                scope.clone(),
                CachedToken {
                    token: token_response.token.clone(),
                    expires_at,
                },
            );
        }
    }

    Ok(token_response.token)
}

/// Fetch one token per known registry covering every blob in `urls`, so the
/// downloads skip the 401 round trip. Failures are logged, not returned: the
/// challenge flow still works without the pre-fetched token.
pub(crate) async fn prefetch_tokens(
    client: &reqwest::Client,
    token_cache: &TokenCache,
    endpoints: &HashMap<String, TokenEndpoint>,
    urls: &[String],
) {
    let mut scopes_by_host: HashMap<&str, BTreeSet<String>> = HashMap::new();
    let mut hosts: Vec<(String, String)> = Vec::new();
    for url in urls {
        if let Some((host, scope)) = registry_scope(url) {
            hosts.push((host, scope));
        }
    }
    for (host, scope) in &hosts {
        if cached_token(token_cache, scope).await.is_none() {
            scopes_by_host
                .entry(host)
                .or_default()
                .insert(scope.clone());
        }
    }

    for (host, scopes) in scopes_by_host {
        let Some(endpoint) = endpoints.get(host) else {
            debug!(
                host,
                "unknown registry; each download will answer its own challenge"
            );
            continue;
        };
        let scopes: Vec<String> = scopes.into_iter().collect();
        match request_token(client, token_cache, endpoint, &scopes).await {
            Ok(_) => debug!(host, scopes = scopes.len(), "pre-fetched registry token"),
            Err(e) => warn!(
                host,
                error = %e.error,
                "failed to pre-fetch registry token; downloads will authenticate one by one"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_scope_supports_core_packages() {
        assert_eq!(
            registry_scope("https://ghcr.io/v2/homebrew/core/lz4/blobs/sha256:abc"),
            Some(("ghcr.io".into(), "repository:homebrew/core/lz4:pull".into()))
        );
    }

    #[test]
    fn registry_scope_supports_tapped_packages_and_ports() {
        assert_eq!(
            registry_scope("https://ghcr.io/v2/hashicorp/tap/terraform/blobs/sha256:abc"),
            Some((
                "ghcr.io".into(),
                "repository:hashicorp/tap/terraform:pull".into()
            ))
        );
        assert_eq!(
            registry_scope("http://127.0.0.1:8080/v2/homebrew/core/jq/blobs/sha256:abc"),
            Some((
                "127.0.0.1:8080".into(),
                "repository:homebrew/core/jq:pull".into()
            ))
        );
    }

    #[test]
    fn registry_scope_rejects_other_urls() {
        assert_eq!(
            registry_scope("https://example.com/bottles/jq.tar.gz"),
            None
        );
        assert_eq!(
            registry_scope("https://ghcr.io/v2/homebrew/core/jq/manifests/1.0"),
            None
        );
        assert_eq!(
            registry_scope("https://ghcr.io/v2//core/jq/blobs/sha256:abc"),
            None
        );
        assert_eq!(registry_scope("not a url"), None);
    }

    #[test]
    fn challenge_parsing_needs_realm_service_and_scope() {
        let (endpoint, scope) = TokenEndpoint::from_challenge(
            "Bearer realm=\"https://ghcr.io/token\",service=\"ghcr.io\",scope=\"repository:homebrew/core/jq:pull\"",
        )
        .unwrap();
        assert_eq!(endpoint.realm, "https://ghcr.io/token");
        assert_eq!(endpoint.service, "ghcr.io");
        assert_eq!(scope, "repository:homebrew/core/jq:pull");

        assert!(TokenEndpoint::from_challenge("Basic realm=\"x\"").is_err());
        assert!(TokenEndpoint::from_challenge("Bearer realm=\"x\",service=\"y\"").is_err());
    }
}
