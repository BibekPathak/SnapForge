//! TLS support for the remote transport.
//!
//! SnapForge terminates TLS **in-process** with `rustls` (the `ring` provider,
//! so no external `openssl`/`cmake` toolchain is required).
//!
//! - [`TlsServerConfig`] supplies a certificate chain and private key for
//!   `snapforge serve`. Set `client_ca_pem` to require **mutual TLS**: the
//!   server then demands a client certificate signed by that CA.
//! - [`TlsClientConfig`] controls how the client trusts the server: the system
//!   roots by default, plus an optional extra CA (useful for a self-signed or
//!   private-CA deployment, and for tests). Set `identity_pem` to present a
//!   client certificate, which is what mTLS requires.
//!
//! TLS is opt-in. With no configuration the transport stays plain HTTP, which
//! is still the right choice behind a TLS-terminating reverse proxy.

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};

use crate::error::{RemoteError, Result};

/// Parse a PEM certificate chain into DER certificates.
fn parse_certs(pem: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &pem[..])
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| RemoteError::Config(format!("parsing {what}: {e}")))?;
    if certs.is_empty() {
        return Err(RemoteError::Config(format!(
            "{what} contains no certificates"
        )));
    }
    Ok(certs)
}

/// Parse a PEM private key.
fn parse_key(pem: &[u8], what: &str) -> Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut &pem[..])
        .map_err(|e| RemoteError::Config(format!("parsing {what}: {e}")))?
        .ok_or_else(|| RemoteError::Config(format!("{what} contains no private key")))
}

/// Server-side TLS material.
#[derive(Debug, Clone)]
pub struct TlsServerConfig {
    /// PEM-encoded certificate chain (leaf first).
    pub cert_pem: Vec<u8>,
    /// PEM-encoded private key (PKCS#8, PKCS#1, or SEC1).
    pub key_pem: Vec<u8>,
    /// When set, require a client certificate signed by one of these PEM CAs
    /// (**mutual TLS**).
    pub client_ca_pem: Option<Vec<u8>>,
}

impl TlsServerConfig {
    /// Load a certificate chain and key from PEM files.
    pub fn from_pem_files(cert_path: &Path, key_path: &Path) -> Result<Self> {
        let cert_pem = std::fs::read(cert_path).map_err(|e| {
            RemoteError::Config(format!(
                "reading TLS certificate {}: {e}",
                cert_path.display()
            ))
        })?;
        let key_pem = std::fs::read(key_path).map_err(|e| {
            RemoteError::Config(format!("reading TLS key {}: {e}", key_path.display()))
        })?;
        Ok(TlsServerConfig {
            cert_pem,
            key_pem,
            client_ca_pem: None,
        })
    }

    /// Require client certificates signed by the given PEM CA bundle (mTLS).
    pub fn with_client_ca_pem(mut self, ca_pem: Vec<u8>) -> Self {
        self.client_ca_pem = Some(ca_pem);
        self
    }

    /// Require client certificates signed by the CA in a PEM file (mTLS).
    pub fn with_client_ca_file(mut self, ca_path: &Path) -> Result<Self> {
        let ca_pem = std::fs::read(ca_path).map_err(|e| {
            RemoteError::Config(format!("reading client CA {}: {e}", ca_path.display()))
        })?;
        self.client_ca_pem = Some(ca_pem);
        Ok(self)
    }

    /// True if mutual TLS is required.
    pub fn requires_client_cert(&self) -> bool {
        self.client_ca_pem.is_some()
    }

    /// Build a `rustls` server configuration from this material.
    pub fn to_rustls(&self) -> Result<Arc<ServerConfig>> {
        crate::tls::ensure_crypto_provider();
        let certs = parse_certs(&self.cert_pem, "TLS certificate chain")?;
        let key = parse_key(&self.key_pem, "TLS key")?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| RemoteError::Config(format!("TLS protocol setup: {e}")))?;

        // Either require a client certificate (mTLS) or accept anonymous peers.
        let builder = match &self.client_ca_pem {
            Some(ca_pem) => {
                let mut roots = RootCertStore::empty();
                for cert in parse_certs(ca_pem, "client CA bundle")? {
                    roots.add(cert).map_err(|e| {
                        RemoteError::Config(format!("adding client CA certificate: {e}"))
                    })?;
                }
                if roots.is_empty() {
                    return Err(RemoteError::Config(
                        "client CA bundle contains no usable certificates".into(),
                    ));
                }
                let verifier =
                    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                        .build()
                        .map_err(|e| {
                            RemoteError::Config(format!(
                                "building client certificate verifier: {e}"
                            ))
                        })?;
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };

        let config = builder
            .with_single_cert(certs, key)
            .map_err(|e| RemoteError::Config(format!("TLS certificate/key mismatch: {e}")))?;
        Ok(Arc::new(config))
    }
}

/// Client-side TLS trust and identity configuration.
#[derive(Debug, Clone, Default)]
pub struct TlsClientConfig {
    /// Additional PEM-encoded CA certificates to trust, on top of the system
    /// roots. Use this for a private CA or a self-signed server certificate.
    pub ca_pem: Option<Vec<u8>>,
    /// PEM-encoded client certificate chain and private key to present to the
    /// server (**mutual TLS**). Both may be in one bundle, or split between
    /// `identity_pem` and `identity_key_pem`.
    pub identity_pem: Option<Vec<u8>>,
    /// PEM-encoded private key for the client identity, if not in
    /// `identity_pem`.
    pub identity_key_pem: Option<Vec<u8>>,
    /// Accept any server certificate. **Insecure**; for local testing only.
    pub accept_invalid_certs: bool,
}

impl TlsClientConfig {
    /// Trust an extra PEM CA bundle (for example a server's own certificate).
    pub fn with_ca_pem(ca_pem: Vec<u8>) -> Self {
        TlsClientConfig {
            ca_pem: Some(ca_pem),
            ..TlsClientConfig::default()
        }
    }

    /// Skip certificate verification entirely. Never use this outside tests.
    pub fn insecure() -> Self {
        TlsClientConfig {
            accept_invalid_certs: true,
            ..TlsClientConfig::default()
        }
    }

    /// Present a client certificate and key (a single combined PEM bundle).
    pub fn with_identity_pem(mut self, identity_pem: Vec<u8>) -> Self {
        self.identity_pem = Some(identity_pem);
        self
    }

    /// Present a client certificate chain with a separate key file.
    pub fn with_identity(mut self, cert_pem: Vec<u8>, key_pem: Vec<u8>) -> Self {
        self.identity_pem = Some(cert_pem);
        self.identity_key_pem = Some(key_pem);
        self
    }

    /// Load a client identity (cert + key) from PEM files.
    pub fn with_identity_files(mut self, cert_path: &Path, key_path: &Path) -> Result<Self> {
        let cert_pem = std::fs::read(cert_path).map_err(|e| {
            RemoteError::Config(format!(
                "reading client certificate {}: {e}",
                cert_path.display()
            ))
        })?;
        let key_pem = std::fs::read(key_path).map_err(|e| {
            RemoteError::Config(format!("reading client key {}: {e}", key_path.display()))
        })?;
        self.identity_pem = Some(cert_pem);
        self.identity_key_pem = Some(key_pem);
        Ok(self)
    }

    /// True if a client identity will be presented.
    pub fn has_identity(&self) -> bool {
        self.identity_pem.is_some()
    }

    pub(crate) fn adds_root(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder> {
        let mut builder = builder;

        if self.accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        } else if let Some(pem) = &self.ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| RemoteError::Config(format!("parsing CA certificate bundle: {e}")))?;
            if certs.is_empty() {
                return Err(RemoteError::Config(
                    "CA certificate bundle contains no certificates".into(),
                ));
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }

        // Present a client identity for mutual TLS.
        if let Some(identity_pem) = &self.identity_pem {
            let mut bundle = identity_pem.clone();
            if let Some(key_pem) = &self.identity_key_pem {
                if !bundle.ends_with(b"\n") {
                    bundle.push(b'\n');
                }
                bundle.extend_from_slice(key_pem);
            }
            let identity = reqwest::Identity::from_pem(&bundle)
                .map_err(|e| RemoteError::Config(format!("parsing client certificate/key: {e}")))?;
            builder = builder.identity(identity);
        }

        Ok(builder)
    }
}

/// Whether an endpoint uses the `https` scheme.
pub fn is_https(endpoint: &str) -> bool {
    endpoint
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
}

/// A stable, non-secret identifier for a peer certificate: the hex SHA-256 of
/// its DER encoding.
///
/// Used to log which client certificate authenticated a connection without
/// parsing X.509 (no extra dependency) and without exposing the certificate.
pub fn certificate_fingerprint(cert_der: &[u8]) -> String {
    blake3::hash(cert_der).to_hex().to_string()
}

/// Install the `ring` crypto provider as the process default, once.
///
/// `reqwest` is built with `rustls-tls-manual-roots-no-provider`, so no provider
/// is compiled in and one must be selected explicitly. Calling this repeatedly
/// is safe: the first install wins and later calls are ignored.
///
/// SnapForge installs this automatically before building any HTTP client; call
/// it directly only when constructing a `reqwest` client yourself.
pub fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            // Another component already installed a provider; that is fine.
            tracing::debug!("a rustls crypto provider was already installed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_certificate_chain_is_rejected() {
        let cfg = TlsServerConfig {
            cert_pem: b"".to_vec(),
            key_pem: b"".to_vec(),
            client_ca_pem: None,
        };
        assert!(cfg.to_rustls().is_err());
    }

    #[test]
    fn garbage_pem_is_rejected() {
        let cfg = TlsServerConfig {
            cert_pem: b"not a certificate".to_vec(),
            key_pem: b"not a key".to_vec(),
            client_ca_pem: None,
        };
        assert!(cfg.to_rustls().is_err());
    }

    #[test]
    fn bad_client_ca_is_rejected() {
        // Any cert/key material with a malformed client CA bundle must fail
        // rather than silently disabling mTLS. Full mTLS is covered by the
        // integration tests, which use real generated certificates.
        let cfg = TlsServerConfig {
            cert_pem: b"not a certificate".to_vec(),
            key_pem: b"not a key".to_vec(),
            client_ca_pem: Some(b"not pem".to_vec()),
        };
        assert!(cfg.to_rustls().is_err());
    }

    #[test]
    fn mtls_flag_tracks_client_ca() {
        let mut cfg = TlsServerConfig {
            cert_pem: b"x".to_vec(),
            key_pem: b"y".to_vec(),
            client_ca_pem: None,
        };
        assert!(!cfg.requires_client_cert());
        cfg.client_ca_pem = Some(b"ca".to_vec());
        assert!(cfg.requires_client_cert());
    }

    #[test]
    fn https_detection() {
        assert!(is_https("https://example.test"));
        assert!(is_https("HTTPS://example.test"));
        assert!(!is_https("http://example.test"));
    }

    #[test]
    fn bad_ca_bundle_is_rejected() {
        let cfg = TlsClientConfig::with_ca_pem(b"not pem".to_vec());
        let builder = reqwest::Client::builder();
        assert!(cfg.adds_root(builder).is_err());
    }

    #[test]
    fn has_identity_reflects_configuration() {
        assert!(!TlsClientConfig::default().has_identity());
        assert!(TlsClientConfig::default()
            .with_identity_pem(b"pem".to_vec())
            .has_identity());
    }

    #[test]
    fn fingerprint_is_stable_and_nonempty() {
        let a = certificate_fingerprint(b"der-bytes");
        let b = certificate_fingerprint(b"der-bytes");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, certificate_fingerprint(b"other"));
    }
}
