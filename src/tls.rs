use crate::config::TlsConfig;
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// Baca cert + key dari disk dan bangun acceptor. Dipakai saat startup dan tiap reload (SIGHUP).
pub fn load(cfg: &TlsConfig) -> Result<TlsAcceptor, String> {
    let certs = CertificateDer::pem_file_iter(&cfg.cert)
        .map_err(|e| format!("tls: tidak bisa membaca cert {}: {e}", cfg.cert))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("tls: cert {} tidak valid: {e}", cfg.cert))?;
    if certs.is_empty() {
        return Err(format!("tls: cert {} tidak berisi sertifikat", cfg.cert));
    }
    let key = PrivateKeyDer::from_pem_file(&cfg.key)
        .map_err(|e| format!("tls: tidak bisa membaca key {}: {e}", cfg.key))?;

    let mut sc = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("tls: cert dan key tidak cocok / tidak valid: {e}"))?;
    sc.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(sc)))
}
