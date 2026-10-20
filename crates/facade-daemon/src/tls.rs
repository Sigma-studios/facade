use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use tokio::net::TcpStream;

pub use facade_core::pinned::fingerprint;

/// The server's own certificate, used on the node and API listeners and as the
/// ingress fallback. Created on first start and kept forever, because agents and
/// CLIs pin its fingerprint.
pub struct Identity {
    pub key: Arc<CertifiedKey>,
    pub fingerprint: String,
}

pub fn load_or_create_identity(dir: &Path, names: Vec<String>) -> anyhow::Result<Identity> {
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    if !cert_path.exists() {
        let mut sans = vec!["facade".to_string(), "localhost".into(), "127.0.0.1".into()];
        sans.extend(names);
        let generated = rcgen::generate_simple_self_signed(sans)?;
        write_private(&key_path, generated.signing_key.serialize_pem().as_bytes())?;
        std::fs::write(&cert_path, generated.cert.pem())?;
    }
    let cert_pem = std::fs::read(&cert_path)?;
    let key_pem = std::fs::read(&key_path)?;
    let key = certified_key(&cert_pem, &key_pem).context("loading server identity")?;
    let fingerprint = fingerprint(key.cert[0].as_ref());
    Ok(Identity { key, fingerprint })
}

pub fn certified_key(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Arc<CertifiedKey>> {
    let certs = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(anyhow!("no certificate in PEM"));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)?;
    Ok(Arc::new(CertifiedKey::new(certs, signing)))
}

/// `notBefore`/`notAfter` of the leaf certificate, as Unix seconds.
pub fn validity(cert_pem: &[u8]) -> anyhow::Result<(u64, u64)> {
    let der = CertificateDer::pem_slice_iter(cert_pem)
        .next()
        .ok_or_else(|| anyhow!("no certificate in PEM"))??;
    let (_, cert) = x509_parser::parse_x509_certificate(der.as_ref())
        .map_err(|e| anyhow!("parsing certificate: {e}"))?;
    let v = cert.validity();
    Ok((
        v.not_before.timestamp().max(0) as u64,
        v.not_after.timestamp().max(0) as u64,
    ))
}

pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)?.write_all(contents)
}

pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A server config always presenting one certificate.
pub fn single_cert_server(key: Arc<CertifiedKey>) -> Arc<rustls::ServerConfig> {
    #[derive(Debug)]
    struct Fixed(Arc<CertifiedKey>);
    impl rustls::server::ResolvesServerCert for Fixed {
        fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }
    Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Fixed(key))),
    )
}

pub type ClientStream = tokio_rustls::client::TlsStream<TcpStream>;

/// Dials `addr` and checks the server presents the certificate with `fingerprint`.
pub async fn connect_pinned(addr: &str, fingerprint: &str) -> anyhow::Result<ClientStream> {
    let verifier = facade_core::pinned::PinnedVerifier::new(Some(fingerprint.to_string()));
    let connector = tokio_rustls::TlsConnector::from(Arc::new(verifier.client_config()));
    let tcp = tokio::time::timeout(std::time::Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .context("connect timed out")??;
    tcp.set_nodelay(true)?;
    let name = rustls::pki_types::ServerName::try_from("facade").expect("valid name");
    Ok(connector.connect(name, tcp).await?)
}
