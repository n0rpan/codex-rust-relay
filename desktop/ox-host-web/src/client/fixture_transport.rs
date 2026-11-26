//! A test binary must explicitly enable this transport before a disposable
//! loopback endpoint can be used. Release binaries always use the standard client.
use super::*;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use std::{
    env, fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
static ENABLED: AtomicBool = AtomicBool::new(cfg!(test));
#[cfg(feature = "fixture-transport")]
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}
pub(super) fn transport(
    origin: &str,
) -> Option<(Client, SocketAddr, tokio_tungstenite::Connector)> {
    if !ENABLED.load(Ordering::Relaxed) || env::var("OX_TEST_ORIGIN").as_deref() != Ok(origin) {
        return None;
    }
    let addr: SocketAddr = env::var("OX_TEST_LOOPBACK").unwrap().parse().unwrap();
    assert!(addr.ip().is_loopback());
    let root = PathBuf::from(env::var_os("OX_TEST_NATIVE_DIR").unwrap());
    let pem = fs::read(root.join("ca.pem")).unwrap();
    let cert = CertificateDer::from_pem_slice(&pem).unwrap();
    let mut anchors = rustls::RootCertStore::empty();
    anchors.add(cert).unwrap();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(anchors)
        .with_no_client_auth();
    let url = url::Url::parse(origin).unwrap();
    let http = Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap())
        .resolve(url.host_str().unwrap(), addr)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    Some((
        http,
        addr,
        tokio_tungstenite::Connector::Rustls(Arc::new(config)),
    ))
}
