//! The certificate handling of ca-certificates' post-install script, done
//! in process: bundles are split the way its `awk` splits them, each
//! certificate is read the way `openssl x509` reads it, and duplicates are
//! dropped by the same SHA-256 fingerprint.

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use x509_parser::prelude::*;

const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
const END: &[u8] = b"-----END CERTIFICATE-----";

/// Cut `input` into certificates as the script's `awk` does: a certificate
/// runs from a line containing `-----BEGIN CERTIFICATE-----` to the next
/// line containing `-----END CERTIFICATE-----`, each line kept verbatim
/// (carriage returns included) with a newline after it. Anything between
/// certificates is dropped, and a `BEGIN` inside a certificate starts over.
pub(super) fn split_certificates(input: &[u8]) -> Vec<Vec<u8>> {
    let mut certificates = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for record in input.split_inclusive(|&byte| byte == b'\n') {
        let line = record.strip_suffix(b"\n").unwrap_or(record);
        if contains(line, BEGIN) {
            current = Some(Vec::new());
        }
        if let Some(certificate) = current.as_mut() {
            certificate.extend_from_slice(line);
            certificate.push(b'\n');
        }
        if contains(line, END)
            && let Some(certificate) = current.take()
        {
            certificates.push(certificate);
        }
    }
    certificates
}

fn contains(line: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(line, needle).is_some()
}

/// A certificate as `openssl x509` reads it from one of the chunks: its DER
/// and the SHA-256 fingerprint `openssl x509 -fingerprint -sha256` prints.
pub(super) struct Certificate<'a> {
    /// The chunk, which is what the script appends to the bundle.
    pub(super) text: &'a [u8],
    pub(super) der: Vec<u8>,
    pub(super) fingerprint: [u8; 32],
}

impl<'a> Certificate<'a> {
    /// `None` where `openssl x509` fails: no `CERTIFICATE` block starting at
    /// a line start, bad base64, or DER that is not a certificate.
    pub(super) fn parse(text: &'a [u8]) -> Option<Self> {
        let (_, pem) = parse_x509_pem(text).ok()?;
        if pem.label != "CERTIFICATE" {
            return None;
        }
        // The fingerprint covers the certificate only, as OpenSSL's does,
        // not anything that follows it in the block.
        let used = X509Certificate::from_der(&pem.contents)
            .ok()
            .map(|(rest, _)| pem.contents.len() - rest.len())?;
        let mut der = pem.contents;
        der.truncate(used);
        let fingerprint = Sha256::digest(&der).into();
        Some(Self {
            text,
            der,
            fingerprint,
        })
    }

    pub(super) fn x509(&self) -> X509Certificate<'_> {
        X509Certificate::from_der(&self.der)
            .map(|(_, cert)| cert)
            .expect("the DER was parsed once already")
    }
}

/// The bundle under construction and the fingerprints already in it.
#[derive(Default)]
pub(super) struct Bundle {
    fingerprints: HashSet<[u8; 32]>,
    content: Vec<u8>,
}

impl Bundle {
    /// The script's `append_unique_certificate`: add `certificate` unless
    /// one with the same fingerprint is already in.
    pub(super) fn append_unique(&mut self, certificate: &Certificate) {
        if self.fingerprints.insert(certificate.fingerprint) {
            self.append_raw(certificate.text);
        }
    }

    /// A chunk as is, no checks: what the script does when it has no
    /// `openssl` to check with.
    pub(super) fn append_raw(&mut self, text: &[u8]) {
        self.content.extend_from_slice(text);
        self.content.push(b'\n');
    }

    /// The script's `append_bundle`: every certificate in `input` that
    /// `openssl` can read, once each.
    pub(super) fn append_bundle(&mut self, input: &[u8]) {
        for chunk in split_certificates(input) {
            if let Some(certificate) = Certificate::parse(&chunk) {
                self.append_unique(&certificate);
            }
        }
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.content
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    pub(in super::super) const ROOT_CA: &[u8] =
        include_bytes!("../../../../fixtures/ca-bundle/root-ca.pem");
    pub(in super::super) const LEAF: &[u8] =
        include_bytes!("../../../../fixtures/ca-bundle/leaf.pem");
    pub(in super::super) const V1_ROOT: &[u8] =
        include_bytes!("../../../../fixtures/ca-bundle/v1-root.pem");

    /// The `split_certificates` function of the script, verbatim.
    const AWK: &str = r#"
          /-----BEGIN CERTIFICATE-----/ {
            certificate = ""
            writing_certificate = 1
          }
          writing_certificate {
            certificate = certificate $0 ORS
          }
          /-----END CERTIFICATE-----/ && writing_certificate {
            output_file = output_dir "/" ++certificate_count ".pem"
            printf "%s", certificate > output_file
            close(output_file)
            writing_certificate = 0
          }
        "#;

    fn crlf(pem: &[u8]) -> Vec<u8> {
        String::from_utf8_lossy(pem)
            .replace('\n', "\r\n")
            .into_bytes()
    }

    /// A bundle with the shapes the inputs take: Mozilla's name lines,
    /// CRLF line endings, a corrupt certificate, a duplicate, a `BEGIN`
    /// inside a line, and no trailing newline.
    pub(in super::super) fn awkward_bundle() -> Vec<u8> {
        let mut corrupt = ROOT_CA.to_vec();
        let middle = corrupt.len() / 2;
        corrupt[middle] = b'!';
        let mut bundle = Vec::new();
        bundle.extend_from_slice(b"Fixture Root CA\n===============\n");
        bundle.extend_from_slice(ROOT_CA);
        bundle.extend_from_slice(b"\nFixture leaf, Windows style\n");
        bundle.extend_from_slice(&crlf(LEAF));
        bundle.extend_from_slice(b"# a corrupt copy\n");
        bundle.extend_from_slice(&corrupt);
        bundle.extend_from_slice(b"# the root again\n");
        bundle.extend_from_slice(ROOT_CA);
        bundle
            .extend_from_slice(b"note: -----BEGIN CERTIFICATE----- in a sentence starts a chunk\n");
        bundle.extend_from_slice(b"that runs to the next end marker, swallowing this line\n");
        bundle.extend_from_slice(b"-----END CERTIFICATE-----\n");
        bundle.extend_from_slice(V1_ROOT);
        bundle.extend_from_slice(b"\n");
        bundle.extend_from_slice(V1_ROOT.strip_suffix(b"\n").unwrap());
        bundle
    }

    #[test]
    fn splits_like_the_script_does() {
        let bundle = awkward_bundle();
        let chunks = split_certificates(&bundle);
        assert_eq!(chunks.len(), 7);
        assert_eq!(chunks[0], ROOT_CA);
        assert_eq!(chunks[1], crlf(LEAF));
        assert_eq!(chunks[3], ROOT_CA);
        assert!(chunks[4].starts_with(b"note: -----BEGIN CERTIFICATE-----"));
        assert_eq!(chunks[5], V1_ROOT);
        assert_eq!(chunks[6], V1_ROOT, "a missing final newline is added");
        let mut restarted = b"-----BEGIN CERTIFICATE-----\nlost\n".to_vec();
        restarted.extend_from_slice(ROOT_CA);
        assert_eq!(
            split_certificates(&restarted),
            [ROOT_CA.to_vec()],
            "a BEGIN starts over"
        );

        assert!(split_certificates(b"").is_empty());
        assert!(split_certificates(b"-----BEGIN CERTIFICATE-----\nunterminated\n").is_empty());

        let Ok(awk) = which("awk") else {
            eprintln!("skipping the awk comparison: awk not found");
            return;
        };
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("bundle.pem");
        fs::write(&input, &bundle).unwrap();
        let status = Command::new(awk)
            .arg("-v")
            .arg(format!("output_dir={}", tmp.path().display()))
            .arg(AWK)
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success());
        for (index, chunk) in chunks.iter().enumerate() {
            let file = tmp.path().join(format!("{}.pem", index + 1));
            assert_eq!(&fs::read(&file).unwrap(), chunk, "chunk {}", index + 1);
        }
        assert!(!tmp.path().join("8.pem").exists());
    }

    #[test]
    fn reads_what_openssl_reads() {
        let root = Certificate::parse(ROOT_CA).unwrap();
        assert_eq!(root.text, ROOT_CA);
        assert_eq!(root.x509().subject().to_string(), "CN=Fixture Root CA");
        let windows = crlf(ROOT_CA);
        let same = Certificate::parse(&windows).unwrap();
        assert_eq!(
            same.fingerprint, root.fingerprint,
            "line endings don't change the DER"
        );
        assert_ne!(
            Certificate::parse(LEAF).unwrap().fingerprint,
            root.fingerprint
        );

        let chunks = split_certificates(&awkward_bundle());
        assert!(Certificate::parse(&chunks[2]).is_none(), "corrupt base64");
        assert!(
            Certificate::parse(&chunks[4]).is_none(),
            "no BEGIN at a line start"
        );
        assert!(
            Certificate::parse(b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
                .is_none()
        );
        assert!(Certificate::parse(b"").is_none());

        let Ok(openssl) = which("openssl") else {
            eprintln!("skipping the openssl comparison: openssl not found");
            return;
        };
        let output = Command::new(openssl)
            .args([
                "x509",
                "-inform",
                "pem",
                "-fingerprint",
                "-sha256",
                "-noout",
                "-in",
            ])
            .arg(fixture("root-ca.pem"))
            .output()
            .unwrap();
        let printed = String::from_utf8(output.stdout).unwrap();
        let hex: String = root
            .fingerprint
            .iter()
            .map(|b| format!("{b:02X}:"))
            .collect();
        assert!(
            printed.to_uppercase().contains(hex.trim_end_matches(':')),
            "{printed} should contain {hex}"
        );
    }

    #[test]
    fn the_bundle_keeps_each_readable_certificate_once() {
        let mut bundle = Bundle::default();
        bundle.append_bundle(&awkward_bundle());
        let mut expected = Vec::new();
        for text in [ROOT_CA.to_vec(), crlf(LEAF), V1_ROOT.to_vec()] {
            expected.extend_from_slice(&text);
            expected.push(b'\n');
        }
        assert_eq!(bundle.into_bytes(), expected);

        let mut raw = Bundle::default();
        raw.append_raw(b"anything\n");
        assert_eq!(raw.into_bytes(), b"anything\n\n");
    }

    pub(in super::super) fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/ca-bundle")
            .join(name)
    }

    pub(in super::super) fn which(program: &str) -> Result<std::path::PathBuf, ()> {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
            .ok_or(())
    }
}
