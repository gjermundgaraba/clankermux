//! The Tailscale local API: the node's tailnet address and its certificate.
//!
//! Two requests do not justify an HTTP client: this is a minimal HTTP/1.1
//! GET with `Connection: close`, reading the response to its end.

use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Where tailscaled listens for the local API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// tailscaled's Unix socket.
    Unix(PathBuf),
    /// The macOS app: loopback TCP with a token shared through a file.
    Tcp { port: u16, token: String },
}

#[derive(Debug, Clone)]
pub struct LocalApi {
    pub endpoint: Endpoint,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// `Retry-After` in seconds, when present.
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

/// The parts of `/localapi/v0/status` the server uses.
#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    #[serde(rename = "Self")]
    pub this: Node,
    #[serde(rename = "CertDomains", default)]
    pub cert_domains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    #[serde(rename = "TailscaleIPs", default)]
    pub tailscale_ips: Vec<IpAddr>,
}

/// A certificate request's outcome.
#[derive(Debug)]
pub enum Cert {
    /// The private key PEM followed by the certificate PEM.
    Pem(Vec<u8>),
    /// A 429: ask again after this long.
    RetryAfter(Duration),
}

const SOCKET: &str = "/var/run/tailscaled.socket";
const MACSYS: &str = "/Library/Tailscale";

impl LocalApi {
    /// Finds tailscaled: its Unix socket, then the macOS system extension
    /// (`/Library/Tailscale/ipnport` names the port, `sameuserproof-<port>`
    /// holds the token), then the App Store app (`sameuserproof-<port>-<token>`
    /// in its group container).
    pub fn discover() -> io::Result<Self> {
        let endpoint = Self::unix_socket()
            .or_else(Self::macsys)
            .or_else(Self::group_container)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no Tailscale local API: looked for {SOCKET}, {MACSYS}/ipnport and \
                         ~/Library/Group Containers/*.io.tailscale.ipn.macsys/sameuserproof-*"
                    ),
                )
            })?;
        Ok(Self { endpoint })
    }

    fn unix_socket() -> Option<Endpoint> {
        let path = PathBuf::from(SOCKET);
        path.exists().then_some(Endpoint::Unix(path))
    }

    fn macsys() -> Option<Endpoint> {
        let dir = PathBuf::from(MACSYS);
        let port: u16 = std::fs::read_link(dir.join("ipnport"))
            .ok()?
            .to_str()?
            .parse()
            .ok()?;
        let token = std::fs::read_to_string(dir.join(format!("sameuserproof-{port}"))).ok()?;
        Some(Endpoint::Tcp {
            port,
            token: token.trim().to_string(),
        })
    }

    fn group_container() -> Option<Endpoint> {
        let home = std::env::var_os("HOME")?;
        let containers = PathBuf::from(home).join("Library/Group Containers");
        for container in std::fs::read_dir(containers).ok()?.flatten() {
            if !container
                .file_name()
                .to_string_lossy()
                .ends_with("io.tailscale.ipn.macsys")
            {
                continue;
            }
            for entry in std::fs::read_dir(container.path()).ok()?.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(rest) = name.strip_prefix("sameuserproof-") else {
                    continue;
                };
                if let Some((port, token)) = rest.split_once('-')
                    && let Ok(port) = port.parse()
                {
                    return Some(Endpoint::Tcp {
                        port,
                        token: token.to_string(),
                    });
                }
            }
        }
        None
    }

    pub async fn get(&self, path: &str) -> io::Result<Response> {
        match &self.endpoint {
            Endpoint::Unix(socket) => {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                get(stream, path, "local-tailscaled.sock", None).await
            }
            Endpoint::Tcp { port, token } => {
                let stream = tokio::net::TcpStream::connect(("127.0.0.1", *port)).await?;
                let host = format!("127.0.0.1:{port}");
                get(stream, path, &host, Some(token)).await
            }
        }
    }

    pub async fn status(&self) -> io::Result<Status> {
        let response = self.get("/localapi/v0/status?peers=false").await?;
        if response.status != 200 {
            return Err(io::Error::other(format!(
                "status: HTTP {}: {}",
                response.status,
                String::from_utf8_lossy(&response.body).trim()
            )));
        }
        serde_json::from_slice(&response.body).map_err(io::Error::other)
    }

    /// The node's certificate for `domain`, renewed by tailscaled when it
    /// is valid for less than `min_validity`.
    pub async fn cert(&self, domain: &str, min_validity: Duration) -> io::Result<Cert> {
        let hours = min_validity.as_secs().div_ceil(3600);
        let response = self
            .get(&format!(
                "/localapi/v0/cert/{domain}?type=pair&min_validity={hours}h"
            ))
            .await?;
        match response.status {
            200 => Ok(Cert::Pem(response.body)),
            429 => Ok(Cert::RetryAfter(
                response.retry_after.unwrap_or(Duration::from_secs(60)),
            )),
            status => Err(io::Error::other(format!(
                "cert: HTTP {status}: {}",
                String::from_utf8_lossy(&response.body).trim()
            ))),
        }
    }
}

/// One GET over `stream`, response read to the end of the stream.
pub async fn get<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    path: &str,
    host: &str,
    token: Option<&str>,
) -> io::Result<Response> {
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(token) = token {
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!(":{token}"));
        request.push_str(&format!("Authorization: Basic {credentials}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    parse(&raw)
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("local API: {what}"))
}

/// Parses a response: the status line, `Retry-After`, and the body, which
/// is de-chunked when `Transfer-Encoding: chunked`.
pub fn parse(raw: &[u8]) -> io::Result<Response> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| invalid("no header end"))?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| invalid("header not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("bad status line"))?;
    let mut retry_after = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("retry-after") {
            retry_after = value.parse().ok().map(Duration::from_secs);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        }
    }
    let mut body = &raw[head_end + 4..];
    let body = if chunked {
        let mut out = Vec::new();
        loop {
            let line_end = body
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or_else(|| invalid("bad chunk"))?;
            let size = std::str::from_utf8(&body[..line_end])
                .ok()
                .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or(""), 16).ok())
                .ok_or_else(|| invalid("bad chunk size"))?;
            body = &body[line_end + 2..];
            if size == 0 {
                break;
            }
            out.extend_from_slice(body.get(..size).ok_or_else(|| invalid("short chunk"))?);
            body = body.get(size + 2..).ok_or_else(|| invalid("short chunk"))?;
        }
        out
    } else {
        body.to_vec()
    };
    Ok(Response {
        status,
        retry_after,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-shot HTTP server: answers the first request with `reply` and
    /// returns what it received.
    async fn serve_once(reply: &'static str) -> (u16, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = vec![0u8; 4096];
            let n = stream.read(&mut received).await.unwrap();
            stream.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&received[..n]).into_owned()
        });
        (port, task)
    }

    #[tokio::test]
    async fn a_request_carries_the_token_and_reads_a_chunked_body() {
        let (port, task) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n\
             3\r\nabc\r\n2;ext\r\nde\r\n0\r\n\r\n",
        )
        .await;
        let api = LocalApi {
            endpoint: Endpoint::Tcp {
                port,
                token: "tok".into(),
            },
        };
        let response = api.get("/localapi/v0/x").await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"abcde");
        let request = task.await.unwrap();
        assert!(
            request.starts_with("GET /localapi/v0/x HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("Authorization: Basic OnRvaw==\r\n"),
            "{request}"
        );
        assert!(request.contains("Connection: close\r\n"));
    }

    #[tokio::test]
    async fn a_rate_limit_carries_its_delay() {
        let (port, _) =
            serve_once("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\n\r\nslow down").await;
        let api = LocalApi {
            endpoint: Endpoint::Tcp {
                port,
                token: String::new(),
            },
        };
        match api
            .cert("x.ts.net", Duration::from_secs(3600))
            .await
            .unwrap()
        {
            Cert::RetryAfter(delay) => assert_eq!(delay, Duration::from_secs(7)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn status_json_yields_addresses_and_domains() {
        let status: Status = serde_json::from_str(
            r#"{"BackendState":"Running","Self":{"TailscaleIPs":["100.1.2.3","fd7a::1"]},"CertDomains":["n.ts.net"]}"#,
        )
        .unwrap();
        assert_eq!(status.this.tailscale_ips.len(), 2);
        assert_eq!(status.cert_domains, vec!["n.ts.net"]);
    }

    #[test]
    fn a_plain_body_runs_to_the_end() {
        let response = parse(b"HTTP/1.1 500 Internal\r\nX: y\r\n\r\nbroken\n").unwrap();
        assert_eq!((response.status, response.retry_after), (500, None));
        assert_eq!(response.body, b"broken\n");
        assert!(parse(b"HTTP/1.1 200").is_err());
    }
}
