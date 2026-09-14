//! Per-user HTTP/CONNECT proxy with OS-authenticated socket ownership.
//! HTTP framing and streamed bodies are handled by Hyper; each request is
//! independently checked, including requests on a persistent connection.
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
};

type ProxyError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, ProxyError>;
type Resolver = Arc<
    dyn Fn(String, u16) -> futures_util::future::BoxFuture<'static, Result<Vec<SocketAddr>>>
        + Send
        + Sync,
>;

type Authorize = Arc<dyn Fn(SocketAddr, SocketAddr) -> bool + Send + Sync>;

pub fn blocked(host: &str, domains: &[String]) -> bool {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    domains.iter().any(|d| {
        let d = d.trim().trim_end_matches('.').to_ascii_lowercase();
        !d.is_empty() && (name == d || name.ends_with(&format!(".{d}")))
    }) || [
        "dns.google",
        "cloudflare-dns.com",
        "dns.quad9.net",
        "dns.nextdns.io",
        "dns.adguard-dns.com",
        "doh.opendns.com",
        "dns.mullvad.net",
    ]
    .iter()
    .any(|d| name == *d || name.ends_with(&format!(".{d}")))
}

pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_unspecified()
                && !v.is_multicast()
                && !v.is_broadcast()
                && v.octets()[0] != 0
                && !(v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1]))
                && v.octets()[0] < 224
        }
        IpAddr::V6(v) => v
            .to_ipv4_mapped()
            .map(|v| public_ip(IpAddr::V4(v)))
            .unwrap_or_else(|| {
                !v.is_loopback()
                    && !v.is_unspecified()
                    && !v.is_multicast()
                    && (v.segments()[0] & 0xfe00 != 0xfc00)
                    && (v.segments()[0] & 0xffc0 != 0xfe80)
            }),
    }
}

pub async fn serve(
    listener: TcpListener,
    domains: watch::Receiver<Vec<String>>,
    authorize: Authorize,
) -> Result<()> {
    serve_with_resolver(
        listener,
        domains,
        authorize,
        Arc::new(|host, port| Box::pin(resolve_public(host, port))),
    )
    .await
}
async fn resolve_public(host: String, port: u16) -> Result<Vec<SocketAddr>> {
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await??
    .collect();
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|a| public_ip(a.ip())),
        "Non-public target"
    );
    Ok(addresses)
}
async fn serve_with_resolver(
    listener: TcpListener,
    domains: watch::Receiver<Vec<String>>,
    authorize: Authorize,
    resolver: Resolver,
) -> Result<()> {
    let limit = Arc::new(Semaphore::new(128));
    loop {
        let (socket, peer) = listener.accept().await?;
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            continue;
        };
        let local = socket.local_addr()?;
        if !peer.ip().is_loopback() || !authorize(peer, local) {
            continue;
        }
        let domains = domains.clone();
        let resolver = resolver.clone();
        let permit = Arc::new(permit);
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let domains = domains.clone();
                let resolver = resolver.clone();
                let permit = permit.clone();
                async move {
                    Ok::<_, Infallible>(match relay(request, domains, permit, resolver).await {
                        Ok(response) => response,
                        Err(e) => {
                            tracing::debug!("Proxy request rejected: {e}");
                            let mut response =
                                Response::new(full("ScreenGuard cannot allow this request."));
                            *response.status_mut() = StatusCode::FORBIDDEN;
                            response
                        }
                    })
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_buf_size(16384)
                .serve_connection(TokioIo::new(socket), service)
                .with_upgrades()
                .await;
        });
    }
}
fn full(text: &str) -> Body {
    Full::new(Bytes::copy_from_slice(text.as_bytes()))
        .map_err(|never| match never {})
        .boxed_unsync()
}
async fn relay(
    mut request: Request<Incoming>,
    mut domains: watch::Receiver<Vec<String>>,
    permit: Arc<OwnedSemaphorePermit>,
    resolver: Resolver,
) -> Result<Response<Body>> {
    let connect = request.method() == hyper::Method::CONNECT;
    let target = request.uri().to_string();
    if connect {
        ensure!(
            !target.contains(['/', '?', '#', '@']),
            "Invalid CONNECT target"
        );
    }
    let url = reqwest::Url::parse(&if connect {
        format!("https://{target}/")
    } else {
        target
    })?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Credentials in URL"
    );
    ensure!(
        if connect {
            url.port_or_known_default() == Some(443)
        } else {
            url.scheme() == "http" && url.port_or_known_default() == Some(80)
        },
        "Only HTTP and HTTPS supported"
    );
    let host = url.host_str().context("No hostname")?.to_owned();
    ensure!(!blocked(&host, &domains.borrow()), "Blocked domain");
    ensure!(
        host.parse::<IpAddr>().is_err() && !host.starts_with('['),
        "IP literal not allowed"
    );
    // Pin the checked resolution to prevent DNS rebinding. Private/local
    // targets are not forwarded with the service's elevated identity.
    let addresses = resolver(host.clone(), if connect { 443 } else { 80 }).await?;
    ensure!(
        !blocked(&host, &domains.borrow()),
        "Policy changed during resolution"
    );
    if connect {
        let mut remote = tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect(addresses.as_slice()),
        )
        .await??;
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(upgraded)) = tokio::time::timeout(Duration::from_secs(10), upgrade).await
            else {
                return;
            };
            let mut client = TokioIo::new(upgraded);
            let transfer = tokio::io::copy_bidirectional(&mut client, &mut remote);
            tokio::pin!(transfer);
            loop {
                tokio::select! {
                    _ = &mut transfer => break,
                    changed = domains.changed() => {
                        if changed.is_err() || blocked(&host, &domains.borrow()) { break; }
                    }
                }
            }
        });
        Ok(Response::new(full("")))
    } else {
        let (parts, body) = request.into_parts();
        let mut headers = parts.headers;
        remove_hop_headers(&mut headers);
        headers.remove("host");
        headers.remove("proxy-authorization");
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .resolve_to_addrs(&host, &addresses)
            .build()?;
        let upstream = client
            .request(parts.method, url)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await?;
        let status = upstream.status();
        let mut headers = upstream.headers().clone();
        remove_hop_headers(&mut headers);
        let stream = upstream.bytes_stream().map(|chunk| {
            chunk
                .map(Frame::data)
                .map_err(|e| Box::new(e) as ProxyError)
        });
        let mut response = Response::new(StreamBody::new(stream).boxed_unsync());
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }
}
fn remove_hop_headers(headers: &mut reqwest::header::HeaderMap) {
    let named: Vec<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|n| n.trim().to_owned()))
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_boundaries_and_case() {
        let list = vec!["YouTube.COM.".into()];
        assert!(blocked("m.youtube.com", &list));
        assert!(blocked("YOUTUBE.COM.", &list));
        assert!(!blocked("notyoutube.com", &list));
        assert!(!blocked("youtube.com.evil.net", &list));
    }
    #[test]
    fn reject_private_and_mapped_targets() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("1.2.3.4".parse().unwrap()));
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    async fn request(allow_peer: bool, target: &str) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_tx, rx) = watch::channel(vec!["blocked.example".into()]);
        let task = tokio::spawn(serve(listener, rx, Arc::new(move |_, _| allow_peer)));
        let mut client = TcpStream::connect(address).await.unwrap();
        let _ = client
            .write_all(
                format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        let mut bytes = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut bytes))
            .await
            .unwrap();
        task.abort();
        bytes
    }
    #[tokio::test]
    async fn another_user_cannot_use_proxy() {
        assert!(request(false, "blocked.example:443").await.is_empty());
    }
    #[tokio::test]
    async fn blocked_connect_is_rejected_without_dns_or_upstream() {
        assert!(
            request(true, "blocked.example:443")
                .await
                .starts_with(b"HTTP/1.1 403")
        );
    }
    #[tokio::test]
    async fn ip_literal_and_non_web_tunnel_are_rejected() {
        assert!(
            request(true, "127.0.0.1:443")
                .await
                .starts_with(b"HTTP/1.1 403")
        );
        assert!(
            request(true, "allowed.example:22")
                .await
                .starts_with(b"HTTP/1.1 403")
        );
    }
}

#[cfg(test)]
mod forwarding_tests {
    use super::*;

    async fn fixture(
        upstream: SocketAddr,
    ) -> (
        SocketAddr,
        watch::Sender<Vec<String>>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = watch::channel(Vec::new());
        // Inject a local fixture instead of contacting a public website. The
        // production entry point always uses resolve_public, which rejects LAN.
        let resolver: Resolver = Arc::new(move |_, _| Box::pin(async move { Ok(vec![upstream]) }));
        let task = tokio::spawn(serve_with_resolver(
            listener,
            rx,
            Arc::new(|_, _| true),
            resolver,
        ));
        (address, tx, task)
    }

    #[tokio::test]
    async fn chunked_upload_and_streamed_response_survive_proxy() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = upstream.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(socket),
                    hyper::service::service_fn(|request: Request<Incoming>| async move {
                        assert_eq!(request.uri().path(), "/upload");
                        // Echo streaming frames; neither proxy nor fixture buffers
                        // the request before responding.
                        Ok::<_, Infallible>(Response::new(request.into_body()))
                    }),
                )
                .await
                .unwrap();
        });
        let (proxy, _policy, task) = fixture(endpoint).await;
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{proxy}")).unwrap())
            .build()
            .unwrap();
        let chunks = vec![
            Bytes::from(vec![b'a'; 65536]),
            Bytes::from(vec![b'b'; 65536]),
        ];
        let body = reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        ));
        let response = client
            .post("http://allowed.example/upload")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.bytes().await.unwrap();
        assert_eq!(bytes.len(), 131072);
        assert!(bytes[..65536].iter().all(|b| *b == b'a'));
        assert!(bytes[65536..].iter().all(|b| *b == b'b'));
        task.abort();
        server.abort();
    }

    #[tokio::test]
    async fn changing_policy_closes_existing_connect_tunnel() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = upstream.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut buf = [0; 4];
            socket.read_exact(&mut buf).await.unwrap();
            socket.write_all(&buf).await.unwrap();
            let _ = socket.read_u8().await;
        });
        let (proxy, policy, task) = fixture(endpoint).await;
        let mut client = TcpStream::connect(proxy).await.unwrap();
        client
            .write_all(b"CONNECT allowed.example:443 HTTP/1.1\r\nHost: allowed.example:443\r\n\r\n")
            .await
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(client.read_u8().await.unwrap());
        }
        assert!(header.starts_with(b"HTTP/1.1 200"));
        client.write_all(b"ping").await.unwrap();
        let mut reply = [0; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");
        policy.send_replace(vec!["allowed.example".into()]);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), client.read_u8())
                .await
                .unwrap()
                .is_err()
        );
        task.abort();
        echo.abort();
    }
}
