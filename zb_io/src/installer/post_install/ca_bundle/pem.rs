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

/// The certificates of `input` in the order the script handles them. Its
/// `awk` writes them to `1.pem`, `2.pem`, ... and the loops then go over
/// `"$output_dir"/*.pem`, which bash sorts as strings: `1`, `10`, `100`,
/// `101`, ... `11`, ... `2`, and so on. That is the order under `LC_ALL=C`,
/// which the step environment sets.
pub(super) fn certificates(input: &[u8]) -> Vec<Vec<u8>> {
    let chunks = split_certificates(input);
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by_cached_key(|index| format!("{}.pem", index + 1));
    let mut chunks: Vec<Option<Vec<u8>>> = chunks.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|index| chunks[index].take().expect("each index once"))
        .collect()
}

fn contains(line: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(line, needle).is_some()
}

/// A chunk this module cannot read that `openssl x509` might: the
/// bundle cannot be built here, the script has to.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Unreadable;

/// Whether `openssl x509` certainly fails on `chunk`, so that dropping it
/// is what the script does: its first line does not start with the
/// `BEGIN` marker (`PEM_read_bio` looks for one at a line start, and the
/// chunk has no other), its body is empty, or the body has a byte outside
/// base64 and whitespace, which the decoder rejects. Anything else that
/// does not parse here might still parse there.
pub(super) fn surely_rejected(chunk: &[u8]) -> bool {
    let lines: Vec<&[u8]> = chunk
        .split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    let Some((first, rest)) = lines.split_first() else {
        return true;
    };
    if !first.starts_with(BEGIN) {
        return true;
    }
    // The last line is the END marker; what's before it is the body.
    let body = rest.iter().take_while(|line| !contains(line, END));
    let mut bytes = 0;
    for line in body {
        for &byte in *line {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'=' => bytes += 1,
                b' ' | b'\t' => {}
                _ => return true,
            }
        }
    }
    bytes == 0
}

/// A certificate as `openssl x509` reads it from one of the chunks: its DER
/// and the SHA-256 fingerprint `openssl x509 -fingerprint -sha256` prints.
pub(super) struct Certificate<'a> {
    /// The chunk, which is what the script appends to the bundle.
    pub(super) text: &'a [u8],
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) der: Vec<u8>,
    pub(super) fingerprint: [u8; 32],
    /// SHA-256 of the SubjectPublicKeyInfo, in hex: what
    /// `openssl x509 -pubkey | openssl pkey -pubin -outform der | openssl
    /// dgst -sha256 -r` prints, which the 2026-10-20 script drops roots by.
    pub(super) key_hash: String,
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
        let (used, key_hash) =
            X509Certificate::from_der(&pem.contents)
                .ok()
                .map(|(rest, cert)| {
                    (
                        pem.contents.len() - rest.len(),
                        hex(&Sha256::digest(cert.public_key().raw)),
                    )
                })?;
        let mut der = pem.contents;
        der.truncate(used);
        let fingerprint = Sha256::digest(&der).into();
        Some(Self {
            text,
            der,
            fingerprint,
            key_hash,
        })
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn x509(&self) -> X509Certificate<'_> {
        X509Certificate::from_der(&self.der)
            .map(|(_, cert)| cert)
            .expect("the DER was parsed once already")
    }
}

/// The bundle under construction and the fingerprints already in it.
pub(super) struct Bundle {
    fingerprints: HashSet<[u8; 32]>,
    /// Keys the script drops on sight, by hex SHA-256 of the
    /// SubjectPublicKeyInfo: the `case` in `append_unique_certificate`
    /// that the 2026-10-20 script added. Empty for the older script.
    excluded_keys: &'static [&'static str],
    content: Vec<u8>,
}

impl Bundle {
    pub(super) fn new(excluded_keys: &'static [&'static str]) -> Self {
        Self {
            fingerprints: HashSet::new(),
            excluded_keys,
            content: Vec::new(),
        }
    }

    /// The script's `append_unique_certificate`: add `certificate` unless
    /// its key is one the script excludes or one with the same fingerprint
    /// is already in.
    pub(super) fn append_unique(&mut self, certificate: &Certificate) {
        if self.excluded_keys.contains(&certificate.key_hash.as_str()) {
            return;
        }
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
    /// `openssl` can read, once each. [`Unreadable`] for a chunk that
    /// does not parse here unless `openssl` would certainly reject it too.
    pub(super) fn append_bundle(&mut self, input: &[u8]) -> Result<(), Unreadable> {
        for chunk in certificates(input) {
            match Certificate::parse(&chunk) {
                Some(certificate) => self.append_unique(&certificate),
                None if surely_rejected(&chunk) => {}
                None => return Err(Unreadable),
            }
        }
        Ok(())
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.content
    }
}

pub(super) fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
    /// Two of the roots the 2026-10-20 script drops by key: Comodo's AAA
    /// Certificate Services, still in Apple's root store, and emSign ECC
    /// Root CA - G3, still in the Mozilla bundle.
    pub(in super::super) const AAA_CERTIFICATE_SERVICES: &[u8] =
        include_bytes!("../../../../fixtures/ca-bundle/aaa-certificate-services.pem");
    pub(in super::super) const EMSIGN_ECC_ROOT_G3: &[u8] =
        include_bytes!("../../../../fixtures/ca-bundle/emsign-ecc-root-ca-g3.pem");

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
    fn orders_certificates_like_the_glob_does() {
        // Twelve certificates: enough for 10, 11 and 12 to sort before 2.
        let mut bundle = Vec::new();
        for _ in 0..4 {
            bundle.extend_from_slice(ROOT_CA);
            bundle.extend_from_slice(LEAF);
            bundle.extend_from_slice(V1_ROOT);
        }
        let in_order = split_certificates(&bundle);
        let globbed = certificates(&bundle);
        assert_eq!(globbed.len(), 12);
        let expected_order = [1, 10, 11, 12, 2, 3, 4, 5, 6, 7, 8, 9];
        for (position, number) in expected_order.into_iter().enumerate() {
            assert_eq!(
                globbed[position],
                in_order[number - 1],
                "position {position}"
            );
        }

        let Ok(bash) = which("bash") else {
            eprintln!("skipping the bash comparison: bash not found");
            return;
        };
        let tmp = TempDir::new().unwrap();
        for (index, chunk) in in_order.iter().enumerate() {
            fs::write(tmp.path().join(format!("{}.pem", index + 1)), chunk).unwrap();
        }
        let output = Command::new(bash)
            .args([
                "-c",
                "for f in \"$1\"/*.pem; do printf '%s\\n' \"${f##*/}\"; done",
                "--",
            ])
            .arg(tmp.path())
            .env_clear()
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        let listed: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        let expected: Vec<String> = expected_order.iter().map(|n| format!("{n}.pem")).collect();
        assert_eq!(listed, expected);
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
        let mut bundle = Bundle::new(&[]);
        bundle.append_bundle(&awkward_bundle()).unwrap();
        let mut expected = Vec::new();
        for text in [ROOT_CA.to_vec(), crlf(LEAF), V1_ROOT.to_vec()] {
            expected.extend_from_slice(&text);
            expected.push(b'\n');
        }
        assert_eq!(bundle.into_bytes(), expected);

        let mut raw = Bundle::new(&[]);
        raw.append_raw(b"anything\n");
        assert_eq!(raw.into_bytes(), b"anything\n\n");
    }

    /// The key hash is what the script's `openssl` pipeline prints, and a
    /// bundle built for the 2026-10-20 script drops the roots it names.
    #[test]
    fn drops_the_keys_the_script_excludes() {
        use super::super::EXCLUDED_KEYS_2026_10_20;
        let aaa = Certificate::parse(AAA_CERTIFICATE_SERVICES).unwrap();
        let emsign = Certificate::parse(EMSIGN_ECC_ROOT_G3).unwrap();
        assert_eq!(
            aaa.key_hash,
            "bd153ed7b0434f6886b17bce8bbe84ed340c7132d702a8f4fa318f756ecbd6f3"
        );
        assert_eq!(
            emsign.key_hash,
            "8d417db2dd8bf5e3084d1e3f196d583849d81bdd4c00c70b9d39369e96b8c782"
        );
        assert!(EXCLUDED_KEYS_2026_10_20.contains(&aaa.key_hash.as_str()));
        assert!(EXCLUDED_KEYS_2026_10_20.contains(&emsign.key_hash.as_str()));
        assert!(
            !EXCLUDED_KEYS_2026_10_20
                .contains(&Certificate::parse(ROOT_CA).unwrap().key_hash.as_str())
        );

        let mut input = Vec::new();
        for text in [AAA_CERTIFICATE_SERVICES, ROOT_CA, EMSIGN_ECC_ROOT_G3] {
            input.extend_from_slice(text);
        }
        let mut older = Bundle::new(&[]);
        older.append_bundle(&input).unwrap();
        assert_eq!(split_certificates(&older.into_bytes()).len(), 3);
        let mut newer = Bundle::new(EXCLUDED_KEYS_2026_10_20);
        newer.append_bundle(&input).unwrap();
        let kept = newer.into_bytes();
        assert_eq!(split_certificates(&kept), [ROOT_CA.to_vec()]);

        let Ok(openssl) = which("openssl") else {
            eprintln!("skipping the openssl comparison: openssl not found");
            return;
        };
        let pipeline = format!(
            "{0} x509 -inform pem -pubkey -noout | {0} pkey -pubin -outform der | {0} dgst -sha256 -r",
            openssl.display()
        );
        let output = Command::new("sh")
            .args(["-c", &pipeline])
            .stdin(fs::File::open(fixture("aaa-certificate-services.pem")).unwrap())
            .output()
            .unwrap();
        let printed = String::from_utf8(output.stdout).unwrap();
        assert!(
            printed.starts_with(&aaa.key_hash),
            "{printed} should start with {}",
            aaa.key_hash
        );
    }

    /// What does not parse here is dropped only where `openssl` would
    /// drop it too. A certificate with its base64 indented parses with
    /// neither, but OpenSSL reads it, so it is left to the script.
    #[test]
    fn leaves_what_openssl_might_read_to_the_script() {
        let chunks = split_certificates(&awkward_bundle());
        assert!(surely_rejected(&chunks[2]), "a byte outside base64");
        assert!(surely_rejected(&chunks[4]), "no BEGIN at a line start");
        assert!(surely_rejected(
            b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n"
        ));
        assert!(!surely_rejected(ROOT_CA));

        let indented: Vec<u8> = String::from_utf8_lossy(ROOT_CA)
            .lines()
            .map(|line| {
                if line.starts_with("-----") {
                    format!("{line}\n")
                } else {
                    format!("    {line}\n")
                }
            })
            .collect::<String>()
            .into_bytes();
        assert!(Certificate::parse(&indented).is_none());
        assert!(!surely_rejected(&indented));
        let mut bundle = Bundle::new(&[]);
        assert_eq!(bundle.append_bundle(&indented), Err(Unreadable));
        assert!(bundle.into_bytes().is_empty());
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
