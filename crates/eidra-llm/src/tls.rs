//! TLS to a model server (docs/DESIGN.md §13): rustls with ring. A server with a certificate of its
//! own, such as `eidra relay --listen` makes, is trusted by a pin: the SHA-256 of that certificate,
//! and no other, at whatever name or address it is reached. A certificate authority is then not
//! needed, nor is a domain name: a LAN address or a public one reach the same server.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

/// Makes ring the process's crypto provider, which reqwest uses for an `https:` server without a
/// pin. Doing it again does nothing.
pub(crate) fn use_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A certificate's SHA-256 as a user may write it: hex, with or without the colons `openssl x509
/// -fingerprint -sha256` puts in, after an optional `sha256:`, or the whole line openssl prints.
pub fn parse_pin(text: &str) -> Result<[u8; 32], String> {
    // openssl prints `sha256 Fingerprint=14:9A:…`.
    let trimmed = text.trim();
    let trimmed = trimmed.split_once("Fingerprint=").map_or(trimmed, |(_, hex)| hex.trim());
    let hex = trimmed.strip_prefix("sha256:").or_else(|| trimmed.strip_prefix("SHA256:"));
    let hex: String = hex.unwrap_or(trimmed).chars().filter(|&c| c != ':').collect();
    let bytes = (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| (0..32).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect())
        .flatten();
    let bytes: Vec<u8> = bytes.ok_or_else(|| {
        format!("the TLS pin {trimmed:?} is not a certificate's SHA-256, 64 hex digits")
    })?;
    Ok(bytes.try_into().expect("32 bytes"))
}

/// A certificate's pin, as eidra writes them: `sha256:` and lowercase hex.
pub fn pin_of(certificate: &[u8]) -> String {
    format!("sha256:{}", hex(&digest(certificate)))
}

/// rustls settings that trust one server certificate, the one with this SHA-256. The handshake is
/// still checked as ever: the server must hold that certificate's private key.
pub fn pinned(pin: [u8; 32]) -> ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Pinned { pin, algorithms: provider.signature_verification_algorithms };
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports rustls's default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth()
}

#[derive(Debug)]
struct Pinned {
    pin: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    /// The certificate itself is the trust, so neither its names nor its dates are looked at.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if digest(end_entity) == self.pin {
            return Ok(ServerCertVerified::assertion());
        }
        Err(rustls::Error::General(format!(
            "the server's certificate is not the pinned one: its pin is {}",
            pin_of(end_entity)
        )))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_read_as_openssl_or_eidra_writes_them() {
        let pin = pin_of(b"a certificate");
        assert!(pin.starts_with("sha256:") && pin.len() == 7 + 64, "{pin}");
        let bytes = parse_pin(&pin).unwrap();
        assert_eq!(parse_pin(&pin[7..]).unwrap(), bytes);
        // openssl x509 -noout -fingerprint -sha256: uppercase, in pairs with colons.
        let openssl: Vec<String> = pin.as_bytes()[7..]
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).to_uppercase())
            .collect();
        assert_eq!(parse_pin(&format!(" SHA256:{} ", openssl.join(":"))).unwrap(), bytes);
        let line = format!("sha256 Fingerprint={}\n", openssl.join(":"));
        assert_eq!(parse_pin(&line).unwrap(), bytes);
        for bad in ["", "sha256:", "abc", &pin[..70], &format!("{}zz", &pin[..69])] {
            assert!(parse_pin(bad).unwrap_err().contains("64 hex digits"), "{bad:?}");
        }
    }
}
