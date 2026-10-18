//! The Rust client over the WebSocket carrier: the writer's flush per
//! frame is what gets `HELLO` through a message carrier.

use client::Connection;
use client::connection::hello;
use proto::*;
use serde_json::json;
use testkit::{Server, connect_ws};

#[tokio::test(flavor = "multi_thread")]
async fn the_rust_client_speaks_over_a_websocket() {
    let server = Server::start(Limits::default());
    let (addr, connector) = server.listen_wss(vec!["http://page.test".into()]).await;
    let ws = connect_ws(addr, &connector, "/", Some("http://page.test"))
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
}
