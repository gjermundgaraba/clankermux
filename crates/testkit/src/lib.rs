mod wire;

pub use wire::*;

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use proto::Limits;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub struct Server {
    pub socket: PathBuf,
    pub registry: Arc<server::Registry>,
    pub dir: tempfile::TempDir,
    handle: tokio::runtime::Handle,
    running: Option<(oneshot::Sender<()>, std::thread::JoinHandle<()>)>,
}

impl Server {
    pub fn start(limits: Limits) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        Server::start_in(limits, dir, state_dir, server::ParkTiming::default())
    }

    pub fn start_with_state(limits: Limits, state_dir: PathBuf) -> Server {
        Server::start_with_park(limits, state_dir, server::ParkTiming::default())
    }

    pub fn start_with_park(limits: Limits, state_dir: PathBuf, park: server::ParkTiming) -> Server {
        Server::start_in(limits, tempfile::tempdir().unwrap(), state_dir, park)
    }

    fn start_in(
        limits: Limits,
        dir: tempfile::TempDir,
        state_dir: PathBuf,
        park: server::ParkTiming,
    ) -> Server {
        let socket = dir.path().join("sock");
        let config = server::Config {
            limits,
            shell: vec!["/bin/sh".into()],
            home: dir.path().to_path_buf(),
            version: "test".into(),
            state_dir,
            socket: server::AbsolutePath::new(&socket).unwrap(),
            park,
        };
        let (stop, stopped) = oneshot::channel();
        let (started, ready) = std::sync::mpsc::channel();
        let listen_on = socket.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let registry = server::Registry::new(config).unwrap();
                let listener = server::bind_unix(&listen_on).unwrap();
                tokio::spawn(server::serve_unix(listener, registry.clone()));
                started
                    .send((registry, tokio::runtime::Handle::current()))
                    .unwrap();
                let _ = stopped.await;
            });
        });
        let (registry, handle) = ready.recv().unwrap();
        Server {
            socket,
            registry,
            dir,
            handle,
            running: Some((stop, thread)),
        }
    }

    async fn listen<F, Fut>(&self, carrier: F) -> SocketAddr
    where
        F: FnOnce(TcpListener) -> Fut + Send + 'static,
        Fut: Future<Output = std::io::Result<()>> + Send + 'static,
    {
        self.handle
            .spawn(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(carrier(listener));
                addr
            })
            .await
            .unwrap()
    }

    pub async fn listen_tcp(&self) -> SocketAddr {
        let registry = self.registry.clone();
        self.listen(move |listener| server::serve_tcp_unauthenticated(listener, registry))
            .await
    }

    pub async fn listen_wss(&self, origins: Vec<String>) -> (SocketAddr, TlsConnector) {
        let (tls, connector) = tls(self.dir.path()).await;
        let registry = self.registry.clone();
        let addr = self
            .listen(move |listener| server::ws::serve_ws(listener, registry, tls, origins))
            .await;
        (addr, connector)
    }

    pub fn shutdown(&mut self) {
        if let Some((stop, thread)) = self.running.take() {
            let _ = stop.send(());
            let _ = thread.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn tls(dir: &Path) -> (Arc<server::tls::Tls>, TlsConnector) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
    let tls = server::tls::Tls::start(server::tls::Source::Files { cert, key })
        .await
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(certified.cert.pem().as_bytes()).unwrap())
        .unwrap();
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    (tls, TlsConnector::from(Arc::new(config)))
}

pub type ClientWs = WebSocketStream<TlsStream<TcpStream>>;

pub async fn connect_ws(
    addr: SocketAddr,
    connector: &TlsConnector,
    path: &str,
    origin: Option<&str>,
) -> Result<ClientWs, WsError> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let _ = tcp.set_nodelay(true);
    let tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let mut request = format!("wss://localhost:{}{path}", addr.port())
        .into_client_request()
        .unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
    }
    tokio_tungstenite::client_async(request, tls)
        .await
        .map(|(ws, _)| ws)
}
