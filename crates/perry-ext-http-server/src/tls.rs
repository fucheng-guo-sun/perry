//! Phase 2 TLS scaffolding — `https.createServer({ key, cert }, ...)`
//! reads PEM-encoded key/cert pairs and builds a `rustls::ServerConfig`.
//! See `https_server::js_node_https_create_server`.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

/// Parse PEM-encoded certificate chain bytes into rustls
/// `CertificateDer`s. Returns an empty vec on parse failure (caller
/// must check for emptiness before building a ServerConfig — empty
/// cert chains fail at TLS-handshake time anyway).
pub fn parse_cert_chain(pem_bytes: &[u8]) -> Vec<CertificateDer<'static>> {
    let mut cursor = std::io::Cursor::new(pem_bytes);
    rustls_pemfile::certs(&mut cursor)
        .filter_map(|c| c.ok())
        .collect()
}

/// Parse a PEM-encoded private key (PKCS#8, RSA, or EC). Returns
/// `None` if the input doesn't yield a recognized key form.
pub fn parse_private_key(pem_bytes: &[u8]) -> Option<PrivateKeyDer<'static>> {
    let mut cursor = std::io::Cursor::new(pem_bytes);
    if let Some(Ok(k)) = rustls_pemfile::pkcs8_private_keys(&mut cursor).next() {
        return Some(PrivateKeyDer::Pkcs8(k));
    }
    let mut cursor = std::io::Cursor::new(pem_bytes);
    if let Some(Ok(k)) = rustls_pemfile::rsa_private_keys(&mut cursor).next() {
        return Some(PrivateKeyDer::Pkcs1(k));
    }
    let mut cursor = std::io::Cursor::new(pem_bytes);
    if let Some(Ok(k)) = rustls_pemfile::ec_private_keys(&mut cursor).next() {
        return Some(PrivateKeyDer::Sec1(k));
    }
    None
}

/// rustls 0.23 requires explicit selection of a CryptoProvider when
/// multiple providers are linked into the binary (perry transitively
/// pulls in both `ring` via our direct dep and `aws-lc-rs` via
/// reqwest's rustls-tls feature). Without an explicit install,
/// `ServerConfig::builder()` panics with "Could not automatically
/// determine the process-level CryptoProvider". Idempotent — the
/// `Once` makes repeated calls safe across multiple createServer
/// invocations within a single process.
fn ensure_crypto_provider_installed() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        // Best-effort install. If a provider was already installed
        // by another crate (or by user code), `install_default()`
        // returns Err; we ignore it because in that case the
        // existing provider is already usable.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build a rustls `ServerConfig` ready for `tokio_rustls::TlsAcceptor`.
/// `alpn_protocols` is set to `[h2, http/1.1]` so an HTTP/2-aware
/// negotiator can pick the upgraded transport on the same port —
/// hooks into the Phase 3 ALPN handoff.
pub fn build_server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    enable_http2: bool,
) -> Result<Arc<ServerConfig>, String> {
    if cert_chain.is_empty() {
        return Err("https.createServer: empty certificate chain".to_string());
    }
    ensure_crypto_provider_installed();
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .map_err(|e| format!("rustls: build server config: {}", e))?;
    if enable_http2 {
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    } else {
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }
    Ok(Arc::new(config))
}
