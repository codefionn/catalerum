//! End-to-end check of [`HttpFetcher`]: a real TCP round-trip against a tiny
//! canned HTTP server, asserting the HTML→Markdown conversion and the SSRF
//! guard's `allow_private_hosts` opt-in (the server is on `127.0.0.1`, which is
//! blocked by default).

use catalerum_core::provider::{FetchFormat, FetchRequest, WebFetcher};
use catalerum_fetch::{FetchPolicy, HttpFetcher};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a one-shot HTTP/1.1 server that replies to every connection with
/// `body` as `text/html`. Returns the bound `http://127.0.0.1:PORT/` base URL.
async fn serve_html(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Serve a handful of connections, then stop.
        for _ in 0..4 {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await; // drain the request line/headers
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    format!("http://{addr}/")
}

/// Spawn a server whose every response is a 302 redirect to `location` (until it
/// stops serving), unless the request path is `/final`, which returns 200 + HTML.
/// Used to exercise manual redirect following and the hop cap.
async fn serve_redirector(location: &'static str, conns: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..conns {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 1024];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let response = if path == "/final" {
                let body = "<html><body><main><p>arrived</p></main></body></html>";
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
            } else {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
            };
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    format!("http://{addr}/")
}

fn local_fetcher() -> HttpFetcher {
    // Opt into private hosts so the loopback test server is reachable.
    let policy = FetchPolicy {
        allow_private_hosts: true,
        ..FetchPolicy::default()
    };
    HttpFetcher::new(None, 10, policy).unwrap()
}

#[tokio::test]
async fn fetches_and_converts_to_markdown() {
    let base = serve_html(
        "<html><head><title>Doc</title></head><body>\
         <nav>menu links here</nav>\
         <main><h1>Heading</h1><p>Hello <b>world</b>.</p></main>\
         <footer>copyright 2026</footer></body></html>",
    )
    .await;

    let fetcher = local_fetcher();
    let page = fetcher.fetch(FetchRequest::new(&base)).await.unwrap();

    assert_eq!(page.status, 200);
    assert_eq!(page.title.as_deref(), Some("Doc"));
    assert_eq!(page.format, FetchFormat::Markdown);
    // Chrome stripped, content converted.
    assert_eq!(page.content, "# Heading\n\nHello **world**.");
    assert!(!page.content.contains("menu"));
    assert!(!page.content.contains("copyright"));
    // The conversion saved context.
    assert!(page.content_bytes < page.raw_bytes);
    assert!(page.context_ratio().unwrap() < 1.0);
}

#[tokio::test]
async fn html_format_returns_raw() {
    let base = serve_html("<html><body><main><p>raw</p></main></body></html>").await;
    let fetcher = local_fetcher();
    let page = fetcher
        .fetch(FetchRequest::new(&base).format(FetchFormat::Html))
        .await
        .unwrap();
    assert!(page.content.contains("<p>raw</p>"));
}

#[tokio::test]
async fn follows_vetted_redirect_chain() {
    // `/` → 302 (relative Location `/final`) → 200. Verifies manual following,
    // the `Url::join` of a relative Location, and that the final url is the target.
    let base = serve_redirector("/final", 4).await;
    let page = local_fetcher()
        .fetch(FetchRequest::new(&base))
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert!(
        page.content.contains("arrived"),
        "followed to /final: {}",
        page.content
    );
    assert!(
        page.url.ends_with("/final"),
        "final url is the hop target: {}",
        page.url
    );
}

#[tokio::test]
async fn caps_infinite_redirect_loop() {
    // Every response 302s back to `/loop` — the fetcher must stop at MAX_REDIRECTS
    // rather than loop forever. (Serve generously more conns than the cap.)
    let base = serve_redirector("/loop", 16).await;
    let err = local_fetcher()
        .fetch(FetchRequest::new(&base))
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("too many redirects"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn custom_resolver_resolves_domain_names() {
    // Fetch via the `localhost` *domain* (not an IP literal) so the custom
    // `GuardedResolver` is actually exercised end-to-end — it must resolve the name
    // and (with the private opt-in) return connectable addresses. An IP-literal
    // fetch skips the resolver entirely, so the other tests don't cover this path.
    let base = serve_html("<html><body><main><p>by-name</p></main></body></html>").await;
    let port = base
        .trim_start_matches("http://127.0.0.1:")
        .trim_end_matches('/');
    let page = local_fetcher()
        .fetch(FetchRequest::new(format!("http://localhost:{port}/")))
        .await
        .unwrap();
    assert_eq!(page.status, 200);
    assert!(
        page.content.contains("by-name"),
        "resolved + fetched: {}",
        page.content
    );
}

#[tokio::test]
async fn loopback_blocked_without_optin() {
    let base = serve_html("<p>secret</p>").await;
    // Default policy denies private/loopback hosts.
    let fetcher = HttpFetcher::with_defaults().unwrap();
    let err = fetcher.fetch(FetchRequest::new(&base)).await.unwrap_err();
    assert!(
        matches!(err, catalerum_core::Error::Unauthorized(_)),
        "got {err:?}"
    );
}
