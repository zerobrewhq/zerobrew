//! The keychain side of the script on macOS. `security find-certificate`
//! exports a keychain, and each certificate is then judged the way
//! `security verify-cert -l -L -c <cert> -p <ssl|basic> -R offline` judges
//! it, with the Security.framework calls that tool makes
//! (SecurityTool/macOS/verify_cert.c): a trust object over the certificate
//! alone, the named policy plus a revocation policy with network access
//! disabled, the default anchors and trust settings. `-l` and `-L` set
//! legacy flags that `SecTrustSetOptions` no longer acts on; they are passed
//! on all the same.

use std::process::Command;

use rayon::prelude::*;
use security_framework::certificate::SecCertificate;
use security_framework::policy::{RevocationPolicy, SecPolicy};
use security_framework::secure_transport::SslProtocolSide;
use security_framework::trust::{SecTrust, TrustOptions};

use super::StepError;

/// The `-p` argument: `ssl` for the System keychain, `basic` for the roots.
#[derive(Clone, Copy)]
pub(super) enum Policy {
    Ssl,
    Basic,
}

/// `/usr/bin/security find-certificate -a -p <keychain>`: every
/// certificate in the keychain, as PEM.
pub(super) fn export_keychain(keychain: &str) -> Result<Vec<u8>, StepError> {
    let output = Command::new("/usr/bin/security")
        .args(["find-certificate", "-a", "-p", keychain])
        .output()
        .map_err(StepError::io("running security find-certificate"))?;
    if !output.status.success() {
        return Err(StepError::Failed(format!(
            "security find-certificate -a -p {keychain}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Whether `security verify-cert` accepts each of `ders` under `policy`,
/// in order. The evaluations are independent, so they run in parallel.
pub(super) fn trusted(ders: &[&[u8]], policy: Policy) -> Vec<bool> {
    ders.par_iter().map(|der| is_trusted(der, policy)).collect()
}

fn is_trusted(der: &[u8], policy: Policy) -> bool {
    let Ok(certificate) = SecCertificate::from_der(der) else {
        return false;
    };
    let policy = match policy {
        Policy::Ssl => SecPolicy::create_ssl(SslProtocolSide::SERVER, None),
        Policy::Basic => SecPolicy::create_x509(),
    };
    let Ok(revocation) = SecPolicy::create_revocation(RevocationPolicy::NETWORK_ACCESS_DISABLED)
    else {
        return false;
    };
    let Ok(mut trust) = SecTrust::create_with_certificates(&[certificate], &[policy, revocation])
    else {
        return false;
    };
    if trust.set_options(TrustOptions::LEAF_IS_CA).is_err() {
        return false;
    }
    trust.evaluate_with_error().is_ok()
}
