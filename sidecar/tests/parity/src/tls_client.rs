//! rustls `ClientConfig` used by the harness driver to accept the
//! test CA's mock certificates.
//!
//! The driver never talks to `https://api.openai.com` directly — it
//! talks to the mocks through the sidecars — but a few cases need
//! direct HTTPS to mock-openai via the published `:50011` port for
//! smoke/debug, and the `egress_http_origin_form_repair` case needs
//! to send raw bytes to the egress port. This config is used wherever
//! the harness needs to validate the test CA chain.
//!
//! Pattern mirrors `sidecar/src/tls.rs` exactly but without the env
//! var fallback — the harness always knows its CA path at boot.

use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::ClientConfig;
use rustls::RootCertStore;
use thiserror::Error;

/// Errors from [`build_harness_client_config`].
#[derive(Debug, Error)]
pub enum TlsClientError {
    #[error("failed to read test CA bundle {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse test CA bundle {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("rustls rejected a certificate in {path}: {reason}")]
    AddCert { path: String, reason: String },
    #[error("no certificates found in {0}")]
    NoCerts(String),
}

/// Build a rustls `ClientConfig` that trusts webpki-roots PLUS the
/// PEM certificates in the given file.
pub fn build_harness_client_config(
    ca_pem_path: impl AsRef<Path>,
) -> Result<Arc<ClientConfig>, TlsClientError> {
    let path = ca_pem_path.as_ref();
    let path_str = path.to_string_lossy().to_string();
    let bytes = std::fs::read(path).map_err(|source| TlsClientError::Io {
        path: path_str.clone(),
        source,
    })?;
    let mut cursor = BufReader::new(bytes.as_slice());
    let certs = rustls_pemfile::certs(&mut cursor)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsClientError::Parse {
            path: path_str.clone(),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsClientError::NoCerts(path_str));
    }

    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in certs {
        root_store.add(cert).map_err(|e| TlsClientError::AddCert {
            path: path_str.clone(),
            reason: e.to_string(),
        })?;
    }

    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_io_error() {
        let err = build_harness_client_config("/definitely/not/a/file.pem").unwrap_err();
        assert!(matches!(err, TlsClientError::Io { .. }));
    }

    #[test]
    fn empty_file_yields_no_certs() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let err = build_harness_client_config(tmp.path()).unwrap_err();
        assert!(matches!(err, TlsClientError::NoCerts(_)));
    }

    #[test]
    fn malformed_pem_yields_parse_or_add_error() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "-----BEGIN CERTIFICATE-----\ngibberish\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let err = build_harness_client_config(tmp.path()).unwrap_err();
        assert!(
            matches!(
                err,
                TlsClientError::Parse { .. }
                    | TlsClientError::AddCert { .. }
                    | TlsClientError::NoCerts(_)
            ),
            "got {err:?}"
        );
    }
}
