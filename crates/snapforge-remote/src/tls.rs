//! TLS support for the remote transport.
//!
//! SnapForge terminates TLS **in-process** with `rustls` (the `ring` provider,
//! so no external `openssl`/`cmake` toolchain is required).
//!
//! - [`TlsServerConfig`] supplies a certificate chain and private key for
//!   `snapforge serve`.
//! - [`TlsClientConfig`] controls how the client trusts the server: the system
//!   roots by default, plus an optional extra CA (useful for a self-signed or
//!   private-CA deployment, and for tests).
//!
//! TLS is opt-in. With no configuration the transport stays plain HTTP, which
//! is still the right choice behind a TLS-terminating reverse proxy.

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

use crate::error::{RemoteError, Result};

/// Server-side TLS material.
#[derive(Debug, Clone)]
pub struct TlsServerConfig {
    /// PEM-encoded certificate chain (leaf first).
    pub cert_pem: Vec<u8>,
    /// PEM-encoded private key (PKCS#8, PKCS#1, or SEC1).
    pub key_pem: Vec<u8>,
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
        Ok(TlsServerConfig { cert_pem, key_pem })
    }

    /// Build a `rustls` server configuration from this material.
    pub fn to_rustls(&self) -> Result<Arc<ServerConfig>> {
        crate::tls::ensure_crypto_provider();
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &self.cert_pem[..])
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| RemoteError::Config(format!("parsing TLS certificate chain: {e}")))?;
        if certs.is_empty() {
            return Err(RemoteError::Config("TLS certificate chain is empty".into()));
        }
        let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &self.key_pem[..])
            .map_err(|e| RemoteError::Config(format!("parsing TLS key: {e}")))?
            .ok_or_else(|| RemoteError::Config("TLS key file contains no private key".into()))?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| RemoteError::Config(format!("TLS protocol setup: {e}")))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| RemoteError::Config(format!("TLS certificate/key mismatch: {e}")))?;
        Ok(Arc::new(config))
    }
}

/// Client-side TLS trust configuration.
#[derive(Debug, Clone, Default)]
pub struct TlsClientConfig {
    /// Additional PEM-encoded CA certificates to trust, on top of the system
    /// roots. Use this for a private CA or a self-signed server certificate.
    pub ca_pem: Option<Vec<u8>>,
    /// Accept any server certificate. **Insecure**; for local testing only.
    pub accept_invalid_certs: bool,
}

impl TlsClientConfig {
    /// Trust an extra PEM CA bundle (for example a server's own certificate).
    pub fn with_ca_pem(ca_pem: Vec<u8>) -> Self {
        TlsClientConfig {
            ca_pem: Some(ca_pem),
            accept_invalid_certs: false,
        }
    }

    /// Skip certificate verification entirely. Never use this outside tests.
    pub fn insecure() -> Self {
        TlsClientConfig {
            ca_pem: None,
            accept_invalid_certs: true,
        }
    }

    pub(crate) fn adds_root(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder> {
        if self.accept_invalid_certs {
            return Ok(builder.danger_accept_invalid_certs(true));
        }
        match &self.ca_pem {
            Some(pem) => {
                let certs = reqwest::Certificate::from_pem_bundle(pem).map_err(|e| {
                    RemoteError::Config(format!("parsing CA certificate bundle: {e}"))
                })?;
                if certs.is_empty() {
                    return Err(RemoteError::Config(
                        "CA certificate bundle contains no certificates".into(),
                    ));
                }
                let mut builder = builder;
                for cert in certs {
                    builder = builder.add_root_certificate(cert);
                }
                Ok(builder)
            }
            None => Ok(builder),
        }
    }
}

/// Whether an endpoint uses the `https` scheme.
pub fn is_https(endpoint: &str) -> bool {
    endpoint
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
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
        };
        assert!(cfg.to_rustls().is_err());
    }

    #[test]
    fn garbage_pem_is_rejected() {
        let cfg = TlsServerConfig {
            cert_pem: b"not a certificate".to_vec(),
            key_pem: b"not a key".to_vec(),
        };
        assert!(cfg.to_rustls().is_err());
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
}
