//! The Rust client over the WebSocket carrier: the writer's flush per
//! frame is what gets `HELLO` through a message carrier.

use std::sync::Arc;

use client::Connection;
use client::connection::hello;
use proto::*;
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test(flavor = "multi_thread")]
async fn the_rust_client_speaks_over_a_websocket() {
    let dir = tempfile::tempdir().unwrap();
    let registry = server::Registry::new(server::Config {
        limits: Limits::default(),
        shell: vec!["/bin/sh".into()],
        home: dir.path().to_path_buf(),
        version: "test".into(),
        state_dir: dir.path().join("state"),
    })
    .unwrap();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, certified.cert.pem()).unwrap();
    std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
    let tls = server::tls::Tls::start(server::tls::Source::Files { cert, key })
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(server::ws::serve_ws(
        listener,
        registry,
        tls,
        vec!["http://page.test".into()],
    ));

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
    let tcp = TcpStream::connect(addr).await.unwrap();
    let stream = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let mut request = format!("wss://localhost:{}/", addr.port())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Origin", "http://page.test".parse().unwrap());
    let (ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    let conn = Connection::handshake(
        server::ws::WsStream::new(ws, Limits::default().max_frame),
        hello("test", Some(ghostty_vt::ENGINE)),
    )
    .await
    .unwrap();
    let session: Session = conn
        .call("create", json!({"label": "over-wss", "argv": ["/bin/cat"]}))
        .await
        .unwrap();
    let page: Page = conn.call("list", json!({})).await.unwrap();
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].id, session.id);
    task.abort();
}
