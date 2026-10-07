use std::sync::{Arc, OnceLock};

use rustls::pki_types::CertificateDer;
use tracing::warn;

static SHARED_TLS_CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

/// Process-wide rustls config used by every reqwest client.
///
/// System trust roots are preferred, but when none are available (e.g. inside
/// a packaging/build sandbox) we fall back to the bundled Mozilla roots from
/// `webpki-roots`. The config is always available, so callers never fall back
/// to reqwest's default TLS (which eagerly loads system roots and panics when
/// none exist).
pub(crate) fn shared_tls_config() -> Arc<rustls::ClientConfig> {
    SHARED_TLS_CONFIG
        .get_or_init(|| Arc::new(build_rustls_config()))
        .clone()
}

fn build_rustls_config() -> rustls::ClientConfig {
    let provider = rustls::crypto::aws_lc_rs::default_provider();

    let cert_result = rustls_native_certs::load_native_certs();
    if !cert_result.errors.is_empty() {
        let details = cert_result
            .errors
            .iter()
            .take(3)
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        warn!(
            errors = cert_result.errors.len(),
            details = %details,
            "failed to load native certificates"
        );
    }

    let root_store = assemble_root_store(cert_result.certs);

    let mut config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        // The aws-lc-rs provider always supports TLS 1.2/1.3, so this is unreachable.
        // Panicking here (instead of returning None) avoids silently dropping back to
        // reqwest's system-root TLS, which would reintroduce the sandbox CA-cert panic.
        .expect("aws-lc-rs provider supports TLS 1.2 and 1.3")
        .with_root_certificates(root_store)
        .with_no_client_auth();
    // reqwest only negotiates HTTP/2 when the TLS config it is handed offers
    // it; without this every connection is HTTP/1.1 and nothing multiplexes.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

/// Assemble a trust store from the system's native certs, falling back to the
/// bundled Mozilla roots (`webpki-roots`) when no native roots are available
/// (e.g. inside a packaging/build sandbox). The returned store is never empty.
fn assemble_root_store(native_certs: Vec<CertificateDer<'static>>) -> rustls::RootCertStore {
    let mut root_store = rustls::RootCertStore::empty();

    for cert in native_certs {
        let _ = root_store.add(cert);
    }

    if root_store.roots.is_empty() {
        warn!("no native CA certificates found; falling back to bundled webpki-roots trust store");
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    root_store
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rustls_config_does_not_panic() {
        let _ = build_rustls_config();
    }

    #[test]
    fn falls_back_to_webpki_roots_when_native_empty() {
        // Simulates a sandbox with no system trust store: the webpki-roots
        // fallback must still produce a non-empty trust store. This is the
        // regression guard for the "No CA certificates were loaded" panic.
        let store = assemble_root_store(Vec::new());
        assert!(!store.roots.is_empty());
    }
}
