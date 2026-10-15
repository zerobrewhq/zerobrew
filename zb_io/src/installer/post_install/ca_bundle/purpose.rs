//! The two `openssl x509` questions the script asks of each keychain
//! certificate: `-checkend 0`, whether it has expired, and whether
//! `-purpose` reports `SSL server CA : Yes`. The second follows OpenSSL's
//! `check_purpose_ssl_server` and `check_ca` in `crypto/x509/v3_purp.c`,
//! which LibreSSL, the `openssl` on macOS, shares.

use x509_parser::oid_registry::OID_X509_EXT_CERT_TYPE;
use x509_parser::prelude::*;

/// Netscape and Microsoft Server Gated Crypto, which OpenSSL counts as
/// server authentication.
const NS_SGC: &str = "2.16.840.1.113730.4.1";
const MS_SGC: &str = "1.3.6.1.4.1.311.10.3.3";

/// `openssl x509 -checkend 0`: expired once `notAfter` is not after `now`.
pub(super) fn is_expired(cert: &X509Certificate, now: i64) -> bool {
    cert.validity().not_after.timestamp() <= now
}

/// Whether `openssl x509 -purpose` prints `SSL server CA : Yes`.
pub(super) fn is_ssl_server_ca(cert: &X509Certificate) -> bool {
    // An extension OpenSSL can't decode makes every purpose "No".
    let (Ok(key_usage), Ok(extended), Ok(basic), Ok(netscape)) = (
        cert.key_usage(),
        cert.extended_key_usage(),
        cert.basic_constraints(),
        cert.get_extension_unique(&OID_X509_EXT_CERT_TYPE),
    ) else {
        return false;
    };
    let (netscape_any_ca, netscape_ssl_ca) = match netscape.map(X509Extension::parsed_extension) {
        None => (false, false),
        Some(ParsedExtension::NSCertType(kinds)) => (
            kinds.ssl_ca() || kinds.smime_ca() || kinds.object_signing_ca(),
            kinds.ssl_ca(),
        ),
        Some(_) => return false,
    };

    // ku_reject(KU_KEY_CERT_SIGN): a key usage must allow signing certificates.
    if key_usage
        .as_ref()
        .is_some_and(|usage| !usage.value.key_cert_sign())
    {
        return false;
    }
    // xku_reject(XKU_SSL_SERVER): an extended key usage must include servers.
    if let Some(extended) = &extended {
        let server = extended.value.server_auth
            || extended.value.other.iter().any(|oid| {
                let oid = oid.to_id_string();
                oid == NS_SGC || oid == MS_SGC
            });
        if !server {
            return false;
        }
    }
    // check_ca, then check_ssl_ca's extra condition on Netscape-only CAs.
    if let Some(basic) = basic {
        basic.value.ca
    } else if cert.version().0 == 0 {
        // OpenSSL takes a self-signed v1 certificate for a root.
        cert.subject().as_raw() == cert.issuer().as_raw()
    } else if key_usage.is_some() {
        true
    } else {
        netscape_any_ca && netscape_ssl_ca
    }
}

#[cfg(test)]
mod tests {
    use super::super::pem::tests::{fixture, which};
    use super::*;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Each fixture, whether it is an SSL server CA, and whether it has
    /// expired. One per branch of `check_ca`.
    const CASES: &[(&str, bool, bool)] = &[
        ("root-ca.pem", true, false),
        ("v1-root.pem", true, false),
        ("ku-only.pem", true, false),
        ("ns-ssl-ca.pem", true, false),
        ("eku-server-ca.pem", true, false),
        ("expired-ca.pem", true, true),
        ("leaf.pem", false, false),
        ("bc-false.pem", false, false),
        ("ku-no-certsign.pem", false, false),
        ("eku-client-ca.pem", false, false),
        ("ns-email-ca.pem", false, false),
    ];

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn classifies_each_branch_of_check_ca() {
        for (name, ssl_server_ca, expired) in CASES {
            let pem = std::fs::read(fixture(name)).unwrap();
            let (_, pem) = parse_x509_pem(&pem).unwrap();
            let cert = pem.parse_x509().unwrap();
            assert_eq!(is_ssl_server_ca(&cert), *ssl_server_ca, "{name}");
            assert_eq!(is_expired(&cert, now()), *expired, "{name}");
        }
    }

    #[test]
    fn agrees_with_openssl() {
        let Ok(openssl) = which("openssl") else {
            eprintln!("skipping the openssl comparison: openssl not found");
            return;
        };
        for (name, ssl_server_ca, expired) in CASES {
            let purpose = Command::new(&openssl)
                .args(["x509", "-noout", "-purpose", "-in"])
                .arg(fixture(name))
                .output()
                .unwrap();
            let purpose = String::from_utf8(purpose.stdout).unwrap();
            assert_eq!(
                purpose.contains("SSL server CA : Yes"),
                *ssl_server_ca,
                "{name}: {purpose}"
            );
            let checkend = Command::new(&openssl)
                .args(["x509", "-noout", "-checkend", "0", "-in"])
                .arg(fixture(name))
                .output()
                .unwrap();
            assert_eq!(!checkend.status.success(), *expired, "{name}");
        }
    }
}
