//! The two `openssl x509` questions the script asks of each keychain
//! certificate: whether `-checkend 0` fails, and whether `-purpose` reports
//! `SSL server CA : Yes`. The `openssl` is `/usr/bin/openssl`, which on
//! macOS is LibreSSL. Its `check_purpose_ssl_server` and `check_ca`
//! (`x509_purp.c`) are OpenSSL's; its time handling is stricter.

use x509_parser::oid_registry::OID_X509_EXT_CERT_TYPE;
use x509_parser::prelude::*;

/// Netscape and Microsoft Server Gated Crypto, which OpenSSL counts as
/// server authentication.
const NS_SGC: &str = "2.16.840.1.113730.4.1";
const MS_SGC: &str = "1.3.6.1.4.1.311.10.3.3";

/// Whether `openssl x509 -checkend 0` fails: `notAfter` is not after
/// `now`, or LibreSSL will not read it at all. `X509_cmp_time` goes
/// through `x509_verify_asn1_time_to_tm`, which applies RFC 5280 4.1.2.5:
/// dates through 2049 must be UTCTime, later ones GeneralizedTime. A
/// certificate that breaks the rule (Certum Trusted Network CA 2 encodes
/// its 2046 expiry as GeneralizedTime) is reported as expiring.
pub(super) fn fails_checkend(cert: &X509Certificate, now: i64) -> bool {
    let not_after = &cert.validity().not_after;
    let well_encoded = if not_after.to_datetime().year() < 2050 {
        not_after.is_utctime()
    } else {
        not_after.is_generalizedtime()
    };
    !well_encoded || not_after.timestamp() <= now
}

/// The answer to [`is_ssl_server_ca`] turns on a name comparison this
/// module does not reproduce; the script has to decide.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Undecided;

/// Whether `openssl x509 -purpose` prints `SSL server CA : Yes`.
pub(super) fn is_ssl_server_ca(cert: &X509Certificate) -> Result<bool, Undecided> {
    // An extension OpenSSL can't decode makes every purpose "No".
    let (Ok(key_usage), Ok(extended), Ok(basic), Ok(netscape)) = (
        cert.key_usage(),
        cert.extended_key_usage(),
        cert.basic_constraints(),
        cert.get_extension_unique(&OID_X509_EXT_CERT_TYPE),
    ) else {
        return Ok(false);
    };
    let (netscape_any_ca, netscape_ssl_ca) = match netscape.map(X509Extension::parsed_extension) {
        None => (false, false),
        Some(ParsedExtension::NSCertType(kinds)) => (
            kinds.ssl_ca() || kinds.smime_ca() || kinds.object_signing_ca(),
            kinds.ssl_ca(),
        ),
        Some(_) => return Ok(false),
    };

    // ku_reject(KU_KEY_CERT_SIGN): a key usage must allow signing certificates.
    if key_usage
        .as_ref()
        .is_some_and(|usage| !usage.value.key_cert_sign())
    {
        return Ok(false);
    }
    // xku_reject(XKU_SSL_SERVER): an extended key usage must include servers.
    if let Some(extended) = &extended {
        let server = extended.value.server_auth
            || extended.value.other.iter().any(|oid| {
                let oid = oid.to_id_string();
                oid == NS_SGC || oid == MS_SGC
            });
        if !server {
            return Ok(false);
        }
    }
    // check_ca, then check_ssl_ca's extra condition on Netscape-only CAs.
    if let Some(basic) = basic {
        Ok(basic.value.ca)
    } else if cert.version().0 == 0 {
        // OpenSSL takes a self-signed v1 certificate for a root: one whose
        // subject equals its issuer under `X509_NAME_cmp`. That compares a
        // canonical form of each name, under which a value encoded as
        // another string type, or in another case, is still the same
        // value. Equal bytes are equal names; bytes that differ are
        // usually different names, and when they might not be, the script
        // decides.
        let (subject, issuer) = (cert.subject(), cert.issuer());
        if subject.as_raw() == issuer.as_raw() {
            Ok(true)
        } else if names_may_match(subject, issuer) {
            Err(Undecided)
        } else {
            Ok(false)
        }
    } else if key_usage.is_some() {
        Ok(true)
    } else {
        Ok(netscape_any_ca && netscape_ssl_ca)
    }
}

/// Whether `X509_NAME_cmp` could call `a` and `b` equal although their
/// bytes differ: the same attributes, each string value the same once
/// folded the way `x509_name_canon` folds it, in ASCII case and in
/// surrounding and repeated whitespace. A value that cannot be read as a
/// string here (a BMPString, say, which the canonical form converts) is
/// taken to match. Approximate on purpose, erring towards "could".
fn names_may_match(a: &X509Name, b: &X509Name) -> bool {
    let fold = |name: &X509Name| -> Option<Vec<Vec<(String, String)>>> {
        name.iter_rdn()
            .map(|rdn| {
                let mut attributes = rdn
                    .iter()
                    .map(|attribute| {
                        let text = attribute.as_str().ok()?;
                        let value = text
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .to_ascii_lowercase();
                        Some((attribute.attr_type().to_id_string(), value))
                    })
                    .collect::<Option<Vec<_>>>()?;
                attributes.sort();
                Some(attributes)
            })
            .collect()
    };
    match (fold(a), fold(b)) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::super::pem::tests::{fixture, which};
    use super::*;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Each fixture, whether it is an SSL server CA, and whether
    /// `-checkend 0` fails. One per branch of `check_ca`, plus the root
    /// whose expiry LibreSSL will not read.
    const CASES: &[(&str, bool, bool)] = &[
        ("certum-trusted-network-ca-2.pem", true, true),
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
            assert_eq!(is_ssl_server_ca(&cert), Ok(*ssl_server_ca), "{name}");
            assert_eq!(fails_checkend(&cert, now()), *expired, "{name}");
        }
    }

    /// A v1 root whose issuer is its subject with the value encoded as
    /// another string type. `X509_NAME_cmp` calls the names equal and
    /// `openssl -purpose` says SSL server CA; the bytes differ, so the
    /// answer is left to the script. A root whose names differ in content
    /// is a plain no.
    #[test]
    fn leaves_a_v1_root_with_differently_encoded_names_to_the_script() {
        let pem = std::fs::read(fixture("v1-root-mixed-names.pem")).unwrap();
        let (_, pem) = parse_x509_pem(&pem).unwrap();
        let cert = pem.parse_x509().unwrap();
        assert_ne!(cert.subject().as_raw(), cert.issuer().as_raw());
        assert_eq!(cert.subject().to_string(), cert.issuer().to_string());
        assert_eq!(is_ssl_server_ca(&cert), Err(Undecided));

        // The same certificate with the issuer's last letter changed to
        // another letter: a different name, not a differently cased one.
        let mut der = pem.contents.clone();
        let issuer_value = cert.issuer().as_raw().to_vec();
        let at = der
            .windows(issuer_value.len())
            .position(|window| window == issuer_value)
            .unwrap();
        der[at + issuer_value.len() - 1] ^= 0x01;
        let (_, other) = X509Certificate::from_der(&der).unwrap();
        assert_ne!(other.subject().to_string(), other.issuer().to_string());
        assert_eq!(is_ssl_server_ca(&other), Ok(false));

        // An issuer whose value is a BMPString: not readable as a string
        // here, converted by the canonical form there.
        let pem = std::fs::read(fixture("v1-root-bmp-issuer.pem")).unwrap();
        let (_, pem) = parse_x509_pem(&pem).unwrap();
        let bmp = pem.parse_x509().unwrap();
        assert_eq!(is_ssl_server_ca(&bmp), Err(Undecided));

        if let Some(openssl) = script_openssl() {
            for name in ["v1-root-mixed-names.pem", "v1-root-bmp-issuer.pem"] {
                let purpose = Command::new(&openssl)
                    .args(["x509", "-noout", "-purpose", "-in"])
                    .arg(fixture(name))
                    .output()
                    .unwrap();
                let purpose = String::from_utf8(purpose.stdout).unwrap();
                assert!(
                    purpose.contains("SSL server CA : Yes"),
                    "{name}: openssl should call the names equal: {purpose}"
                );
            }
        }
    }

    /// The `openssl` the script runs: `/usr/bin/openssl` where it exists,
    /// which on macOS is LibreSSL, else whichever is on the PATH.
    fn script_openssl() -> Option<std::path::PathBuf> {
        let openssl = std::path::PathBuf::from("/usr/bin/openssl");
        if openssl.is_file() {
            Some(openssl)
        } else {
            which("openssl").ok()
        }
    }

    /// Against the `openssl` the script runs. OpenSSL reads the times
    /// LibreSSL rejects, so `-checkend` is only compared with LibreSSL.
    #[test]
    fn agrees_with_openssl() {
        let Some(openssl) = script_openssl() else {
            eprintln!("skipping the openssl comparison: openssl not found");
            return;
        };
        let version = Command::new(&openssl).arg("version").output().unwrap();
        let libressl = String::from_utf8_lossy(&version.stdout).starts_with("LibreSSL");
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
            if libressl {
                let checkend = Command::new(&openssl)
                    .args(["x509", "-noout", "-checkend", "0", "-in"])
                    .arg(fixture(name))
                    .output()
                    .unwrap();
                assert_eq!(!checkend.status.success(), *expired, "{name}");
            }
        }
    }
}
