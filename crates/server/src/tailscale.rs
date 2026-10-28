use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone)]
enum Endpoint {
    Unix(PathBuf),
    Tcp { port: u16, token: String },
}

#[derive(Debug, Clone)]
pub struct LocalApi {
    endpoint: Endpoint,
}

#[derive(Debug)]
struct Response {
    status: u16,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

#[derive(Debug, Deserialize)]
pub struct Status {
    #[serde(rename = "Self")]
    pub this: Node,
    #[serde(rename = "CertDomains", default)]
    pub cert_domains: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Node {
    #[serde(rename = "TailscaleIPs", default)]
    pub tailscale_ips: Vec<IpAddr>,
}

#[derive(Debug)]
pub enum Cert {
    /// The private key PEM followed by the certificate PEM.
    Pem(Vec<u8>),
    RetryAfter(Duration),
}

const SOCKET: &str = "/var/run/tailscaled.socket";
const MACSYS: &str = "/Library/Tailscale";
/// The App Store app's group container, `<team id>.io.tailscale.ipn.macos`.
const MACOS_CONTAINER: &str = "io.tailscale.ipn.macos";
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// A certificate request may wait on an ACME issuance.
const CERT_TIMEOUT: Duration = Duration::from_secs(120);

impl LocalApi {
    /// Finds tailscaled: its Unix socket, then the App Store app
    /// (`sameuserproof-<port>-<token>` in its group container), then the
    /// macOS system extension (`/Library/Tailscale/ipnport` names the
    /// port, `sameuserproof-<port>` holds the token).
    pub fn discover() -> io::Result<Self> {
        let endpoint = Self::unix_socket()
            .or_else(Self::group_container)
            .map(Ok)
            .or_else(|| Self::macsys().transpose())
            .unwrap_or_else(|| {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no Tailscale local API: looked for {SOCKET}, \
                         ~/Library/Group Containers/*.{MACOS_CONTAINER}/sameuserproof-* \
                         and {MACSYS}/ipnport"
                    ),
                ))
            })?;
        Ok(Self { endpoint })
    }

    fn unix_socket() -> Option<Endpoint> {
        let path = PathBuf::from(SOCKET);
        path.exists().then_some(Endpoint::Unix(path))
    }

    /// None without `ipnport`; an error when the proof beside it cannot be
    /// read (it is readable by the `admin` group).
    fn macsys() -> io::Result<Option<Endpoint>> {
        let dir = Path::new(MACSYS);
        let Ok(port) = std::fs::read_link(dir.join("ipnport")) else {
            return Ok(None);
        };
        let port: u16 = port
            .to_str()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| invalid(&format!("{MACSYS}/ipnport is not a port")))?;
        let proof = dir.join(format!("sameuserproof-{port}"));
        let token = std::fs::read_to_string(&proof)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", proof.display())))?;
        Ok(Some(Endpoint::Tcp {
            port,
            token: token.trim().to_string(),
        }))
    }

    fn group_container() -> Option<Endpoint> {
        let home = std::env::var_os("HOME")?;
        let containers = PathBuf::from(home).join("Library/Group Containers");
        for container in std::fs::read_dir(containers).ok()?.flatten() {
            if !container
                .file_name()
                .to_string_lossy()
                .ends_with(MACOS_CONTAINER)
            {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(container.path()) else {
                continue;
            };
            for entry in entries.flatten() {
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

    async fn get(&self, path: &str, timeout: Duration) -> io::Result<Response> {
        let request = async {
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
        };
        tokio::time::timeout(timeout, request)
            .await
            .unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("local API: no answer to {path} in {}s", timeout.as_secs()),
                ))
            })
    }

    pub async fn status(&self) -> io::Result<Status> {
        let response = self
            .get("/localapi/v0/status?peers=false", STATUS_TIMEOUT)
            .await?;
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
            .get(
                &format!("/localapi/v0/cert/{domain}?type=pair&min_validity={hours}h"),
                CERT_TIMEOUT,
            )
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

async fn get<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    path: &str,
    host: &str,
    token: Option<&str>,
) -> io::Result<Response> {
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(token) = token {
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!(":{token}"));
        request.push_str("Authorization: Basic ");
        request.push_str(&credentials);
        request.push_str("\r\n");
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

fn parse(raw: &[u8]) -> io::Result<Response> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    let head_len = match response.parse(raw) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return Err(invalid("truncated response")),
        Err(e) => return Err(invalid(&e.to_string())),
    };
    let status = response.code.ok_or_else(|| invalid("no status"))?;
    let mut retry_after = None;
    let mut chunked = false;
    for header in response.headers.iter() {
        let value = String::from_utf8_lossy(header.value);
        if header.name.eq_ignore_ascii_case("retry-after") {
            retry_after = value.trim().parse().ok().map(Duration::from_secs);
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .rsplit(',')
                .next()
                .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
        }
    }
    let mut body = &raw[head_len..];
    let body = if chunked {
        let mut out = Vec::new();
        loop {
            let Ok(httparse::Status::Complete((skip, size))) = httparse::parse_chunk_size(body)
            else {
                return Err(invalid("bad chunk size"));
            };
            let size = usize::try_from(size).map_err(|_| invalid("bad chunk size"))?;
            body = &body[skip..];
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

    fn api(port: u16, token: &str) -> LocalApi {
        LocalApi {
            endpoint: Endpoint::Tcp {
                port,
                token: token.into(),
            },
        }
    }

    #[tokio::test]
    async fn a_request_carries_the_token_and_reads_a_chunked_body() {
        let (port, task) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: gzip, chunked\r\n\r\n\
             3\r\nabc\r\n2 ;ext\r\nde\r\n0\r\n\r\n",
        )
        .await;
        let response = api(port, "tok")
            .get("/localapi/v0/x", STATUS_TIMEOUT)
            .await
            .unwrap();
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
        match api(port, "")
            .cert("x.ts.net", Duration::from_secs(3600))
            .await
            .unwrap()
        {
            Cert::RetryAfter(delay) => assert_eq!(delay, Duration::from_secs(7)),
            other @ Cert::Pem(_) => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_silent_daemon_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hold = tokio::spawn(async move { listener.accept().await.unwrap() });
        let error = api(port, "")
            .get("/localapi/v0/x", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        hold.abort();
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
