//! The WebSocket carrier's certificate: from files or from Tailscale,
//! renewed on a timer.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::tailscale::{Cert, LocalApi};

/// Tailscaled renews a certificate valid for less than this.
const MIN_VALIDITY: Duration = Duration::from_secs(7 * 24 * 3600);
const RENEW_EVERY: Duration = Duration::from_secs(12 * 3600);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub enum Source {
    /// PEM files, re-read at every renewal.
    Files { cert: PathBuf, key: PathBuf },
    /// `/localapi/v0/cert/<domain>`.
    Tailscale { api: LocalApi, domain: String },
}

impl Source {
    async fn fetch(&self) -> io::Result<Cert> {
        match self {
            Source::Files { cert, key } => {
                let mut pem = std::fs::read(key)?;
                pem.extend(std::fs::read(cert)?);
                Ok(Cert::Pem(pem))
            }
            Source::Tailscale { api, domain } => api.cert(domain, MIN_VALIDITY).await,
        }
    }
}

/// A server configuration from PEM holding the private key and the
/// certificate chain, in any order.
fn config(pem: &[u8]) -> io::Result<Arc<ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<_, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("certificate: {e}")))?;
    let key = PrivateKeyDer::from_pem_slice(pem)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("private key: {e}")))?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| b.with_no_client_auth().with_single_cert(certs, key))
        .map(Arc::new)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("TLS: {e}")))
}

/// The current certificate; the acceptor is built per connection so a
/// renewal takes effect at the next one.
pub struct Tls {
    current: Mutex<Arc<ServerConfig>>,
    source: Source,
}

impl Tls {
    /// Fetches the certificate once; a failure fails the carrier.
    pub async fn start(source: Source) -> io::Result<Arc<Self>> {
        let config = match source.fetch().await? {
            Cert::Pem(pem) => config(&pem)?,
            Cert::RetryAfter(delay) => {
                return Err(io::Error::other(format!(
                    "certificate rate limited; retry after {}s",
                    delay.as_secs()
                )));
            }
        };
        Ok(Arc::new(Self {
            current: Mutex::new(config),
            source,
        }))
    }

    pub fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(self.current.lock().unwrap().clone())
    }

    /// Renews on a timer; a rate limit waits as told, another failure
    /// keeps the current certificate and tries again later.
    pub async fn renew_forever(self: Arc<Self>) {
        let mut wait = RENEW_EVERY;
        loop {
            tokio::time::sleep(wait).await;
            let fetched = match self.source.fetch().await {
                Ok(Cert::Pem(pem)) => config(&pem),
                Ok(Cert::RetryAfter(delay)) => {
                    tracing::info!(seconds = delay.as_secs(), "certificate rate limited");
                    wait = delay;
                    continue;
                }
                Err(error) => Err(error),
            };
            wait = match fetched {
                Ok(config) => {
                    *self.current.lock().unwrap() = config;
                    tracing::info!("certificate renewed");
                    RENEW_EVERY
                }
                Err(error) => {
                    tracing::warn!(%error, "certificate renewal failed; keeping the current one");
                    RETRY_AFTER_FAILURE
                }
            };
        }
    }
}
