//! WebDAV [`StorageBackend`] (SOUL §9): blobs live in a WebDAV collection
//! (Nextcloud, `rclone serve webdav`, Apache `mod_dav`, …), addressed by object
//! **key**; only catalogued metadata lands in Postgres (§10). The same
//! `catalerum-core` trait the local-filesystem and S3 backends implement.
//!
//! Built on plain HTTP verbs — `PUT`/`GET`/`DELETE` plus the WebDAV `PROPFIND`
//! (list/stat) and `MKCOL` (create collection). Reads/writes buffer the whole
//! object (mirrors the other backends; chunked streaming is a later refinement).

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use futures::stream::{self, BoxStream, StreamExt};
use reqwest::{Client, Method, StatusCode, Url};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{ByteStream, ObjectMeta, PutMeta, StorageBackend};

/// The `PROPFIND` request body — the props the listing/stat needs.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop>
<getcontentlength/><getlastmodified/><getetag/><getcontenttype/><resourcetype/>
</prop></propfind>"#;

/// A [`StorageBackend`] over a WebDAV collection rooted at `base`.
#[derive(Clone, Debug)]
pub struct WebDavBackend {
    client: Client,
    /// Collection root URL, normalised to a single trailing slash.
    base: Url,
    /// Optional HTTP-basic credentials.
    auth: Option<(String, String)>,
}

impl WebDavBackend {
    /// A backend at `base_url` (the collection root, e.g. `http://host:8080/`), with
    /// optional HTTP-basic credentials (empty `username` = anonymous). Errors on an
    /// unparseable base URL.
    pub fn new(base_url: &str, username: &str, password: &str) -> Result<Self> {
        let mut s = base_url.trim_end_matches('/').to_string();
        s.push('/'); // a trailing slash so path joins land *under* the collection
        let base = Url::parse(&s)
            .map_err(|e| Error::invalid(format!("invalid webdav base url `{base_url}`: {e}")))?;
        let auth = (!username.is_empty()).then(|| (username.to_string(), password.to_string()));
        // A connect timeout so an unreachable WebDAV host fails fast instead of
        // hanging a storage/ingest call. Deliberately *no* overall request timeout:
        // GET/PUT stream object bodies that can be large and legitimately slow, and
        // an overall cap would abort a transfer that is still making progress.
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(map_req)?;
        Ok(Self { client, base, auth })
    }

    /// The absolute URL for `key`, percent-encoding each path segment. Rejects a
    /// traversing / empty key first (parity with the local backend's guard), so a
    /// `..` can never escape the workspace prefix regardless of URL-join quirks.
    fn url_for(&self, key: &str) -> Result<Url> {
        validate_key(key)?;
        let mut url = self.base.clone();
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|()| Error::invalid("webdav base url cannot be a base"))?;
            segs.pop_if_empty(); // drop the base's trailing empty segment
            for part in key.split('/').filter(|s| !s.is_empty()) {
                segs.push(part);
            }
        }
        Ok(url)
    }

    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let req = self.client.request(method, url);
        match &self.auth {
            Some((u, p)) => req.basic_auth(u, Some(p)),
            None => req,
        }
    }

    /// `MKCOL` each ancestor collection of `key` top-down (idempotent — an existing
    /// collection answers `405`/`301`, treated as success), so a `PUT` to a nested
    /// key whose parent dirs don't exist yet succeeds.
    async fn ensure_parents(&self, key: &str) -> Result<()> {
        let parts: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
        let mut acc = String::new();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            let url = self.url_for(&acc)?;
            self.request(mkcol(), url).send().await.map_err(map_req)?;
            // Any status is tolerated here (201 created / 405 exists / 301); a truly
            // missing path surfaces as a clear error on the subsequent PUT.
        }
        Ok(())
    }
}

#[async_trait]
impl StorageBackend for WebDavBackend {
    async fn list(&self, prefix: &str) -> Result<BoxStream<'static, Result<ObjectMeta>>> {
        // Walk the tree with **Depth:1** PROPFINDs (universally supported, unlike
        // Depth:infinity which Apache/Nextcloud forbid by default), collecting files;
        // filter by prefix (mirrors the local backend's full-walk + `starts_with`).
        // Branches that can't contain the prefix are pruned.
        let base_path = self.base.path().to_string();
        let mut out: Vec<ObjectMeta> = Vec::new();
        let mut dirs: Vec<String> = vec![String::new()]; // "" = the root collection
        while let Some(dir) = dirs.pop() {
            let url = if dir.is_empty() {
                self.base.clone()
            } else {
                self.url_for(&dir)?
            };
            let resp = self
                .request(propfind(), url)
                .header("Depth", "1")
                .header(reqwest::header::CONTENT_TYPE, "application/xml")
                .body(PROPFIND_BODY)
                .send()
                .await
                .map_err(map_req)?;
            if resp.status() == StatusCode::NOT_FOUND {
                continue;
            }
            if !resp.status().is_success() {
                return Err(Error::provider(format!(
                    "webdav PROPFIND list: {}",
                    resp.status()
                )));
            }
            let xml = resp.text().await.map_err(map_req)?;
            for entry in parse_multistatus(&xml, &base_path)? {
                // Depth:1 echoes the queried directory itself — skip it.
                if entry.meta.key == dir {
                    continue;
                }
                if entry.is_dir {
                    // Recurse only into branches on the path to / under the prefix.
                    if should_descend(&entry.meta.key, prefix) {
                        dirs.push(entry.meta.key);
                    }
                } else {
                    out.push(entry.meta);
                }
            }
        }
        out.retain(|m| m.key.starts_with(prefix));
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(stream::iter(out.into_iter().map(Ok)).boxed())
    }

    async fn stat(&self, key: &str) -> Result<ObjectMeta> {
        let url = self.url_for(key)?;
        let resp = self
            .request(propfind(), url)
            .header("Depth", "0")
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(PROPFIND_BODY)
            .send()
            .await
            .map_err(map_req)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(Error::NotFound);
        }
        if !resp.status().is_success() {
            return Err(Error::provider(format!(
                "webdav PROPFIND stat {key}: {}",
                resp.status()
            )));
        }
        let xml = resp.text().await.map_err(map_req)?;
        let base_path = self.base.path().to_string();
        // Depth:0 returns this resource only; a collection isn't an object → NotFound.
        let mut meta = parse_multistatus(&xml, &base_path)?
            .into_iter()
            .find(|e| !e.is_dir)
            .map(|e| e.meta)
            .ok_or(Error::NotFound)?;
        meta.key = key.to_string();
        Ok(meta)
    }

    async fn get(&self, key: &str) -> Result<ByteStream> {
        let url = self.url_for(key)?;
        let resp = self
            .request(Method::GET, url)
            .send()
            .await
            .map_err(map_req)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(Error::NotFound);
        }
        let resp = resp.error_for_status().map_err(map_req)?;
        let bytes = resp.bytes().await.map_err(map_req)?.to_vec();
        Ok(stream::once(async move { Ok(bytes) }).boxed())
    }

    async fn put(&self, key: &str, mut data: ByteStream, meta: PutMeta) -> Result<()> {
        let mut buf = Vec::new();
        while let Some(chunk) = data.next().await {
            buf.extend_from_slice(&chunk?);
        }
        self.ensure_parents(key).await?;
        let url = self.url_for(key)?;
        let mut req = self.request(Method::PUT, url).body(buf);
        if let Some(ct) = meta.content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, ct);
        }
        req.send()
            .await
            .map_err(map_req)?
            .error_for_status()
            .map_err(map_req)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let url = self.url_for(key)?;
        let resp = self
            .request(Method::DELETE, url)
            .send()
            .await
            .map_err(map_req)?;
        // Idempotent: a missing resource (404) is success.
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        resp.error_for_status().map_err(map_req)?;
        Ok(())
    }

    async fn ensure_container(&self) -> Result<()> {
        // A WebDAV server's root *is* the served collection — it always exists and
        // can't be created (MKCOL on the root errors on most servers). So just verify
        // the root is reachable with a shallow PROPFIND; the collection is a given.
        let st = self
            .request(propfind(), self.base.clone())
            .header("Depth", "0")
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(PROPFIND_BODY)
            .send()
            .await
            .map_err(map_req)?
            .status();
        if st.is_success() {
            Ok(())
        } else {
            Err(Error::provider(format!("webdav root not reachable: {st}")))
        }
    }
}

fn propfind() -> Method {
    Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid method token")
}

fn mkcol() -> Method {
    Method::from_bytes(b"MKCOL").expect("MKCOL is a valid method token")
}

fn map_req(e: reqwest::Error) -> Error {
    Error::provider(format!("webdav request: {e}"))
}

/// One parsed `multistatus` resource: its [`ObjectMeta`] and whether it is a
/// collection (directory). `list` recurses into dirs; `stat` ignores them.
struct Entry {
    is_dir: bool,
    meta: ObjectMeta,
}

/// Parse a WebDAV `multistatus` body into one [`Entry`] per resource. `base_path`
/// is the collection root path, stripped from each `href` to recover the object
/// key. Properties are read **only from `propstat` blocks with a 2xx status** (and
/// merged across them) — a `404` propstat holds *absent* requested props with empty
/// placeholder values, so reading those would yield a phantom `size = 0`.
fn parse_multistatus(xml: &str, base_path: &str) -> Result<Vec<Entry>> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| Error::provider(format!("webdav xml parse: {e}")))?;
    let mut out = Vec::new();
    for response in doc
        .descendants()
        .filter(|n| n.has_tag_name(("DAV:", "response")))
    {
        let Some(href) = response
            .children()
            .find(|n| n.has_tag_name(("DAV:", "href")))
            .and_then(|n| n.text())
        else {
            continue;
        };
        let key = href_to_key(href, base_path);
        if key.is_empty() {
            continue;
        }
        let is_dir = response
            .descendants()
            .any(|n| n.has_tag_name(("DAV:", "collection")));
        // The `<prop>` blocks from every 2xx `<propstat>` (a missing-prop `<propstat>`
        // carries a 4xx status and empty placeholders — excluded).
        let ok_props: Vec<roxmltree::Node> = response
            .children()
            .filter(|n| n.has_tag_name(("DAV:", "propstat")))
            .filter(|ps| propstat_is_ok(*ps))
            .filter_map(|ps| ps.children().find(|n| n.has_tag_name(("DAV:", "prop"))))
            .collect();
        let get = |tag: &str| {
            ok_props
                .iter()
                .find_map(|p| {
                    p.children()
                        .find(|n| n.has_tag_name(("DAV:", tag)))
                        .and_then(|n| n.text())
                })
                .map(str::trim)
        };
        out.push(Entry {
            is_dir,
            meta: ObjectMeta {
                key,
                size: get("getcontentlength")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                etag: get("getetag").map(|s| s.trim_matches('"').to_string()),
                content_type: get("getcontenttype")
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                last_modified: get("getlastmodified")
                    .and_then(parse_http_date)
                    .unwrap_or_else(Utc::now),
            },
        });
    }
    Ok(out)
}

/// True if a `<propstat>`'s `<status>` line is a 2xx (or absent — be lenient).
fn propstat_is_ok(propstat: roxmltree::Node) -> bool {
    let Some(status) = propstat
        .children()
        .find(|n| n.has_tag_name(("DAV:", "status")))
        .and_then(|n| n.text())
    else {
        return true;
    };
    // e.g. "HTTP/1.1 200 OK" → the 2nd whitespace token is the code.
    status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .is_some_and(|c| (200..300).contains(&c))
}

/// Reject a traversing / empty object key (parity with the local backend, since
/// WebDAV keys are otherwise just URL path bytes): no `.`/`..` segment, and at
/// least one normal segment. Empty segments (double/leading slashes) collapse.
fn validate_key(key: &str) -> Result<()> {
    let mut any = false;
    for seg in key.split('/') {
        match seg {
            "" => {}
            "." | ".." => return Err(Error::invalid(format!("invalid object key `{key}`"))),
            _ => any = true,
        }
    }
    any.then_some(())
        .ok_or_else(|| Error::invalid("object key must not be empty"))
}

/// The object key for a PROPFIND `href`: take its path, strip the collection
/// `base_path` prefix, and percent-decode **per segment** (so an encoded `%2F`
/// inside a name stays within that segment rather than splitting the key).
/// Whether the [`list`](WebDavBackend::list) walk should descend into directory
/// `dir` when listing under `prefix`. Descend if `dir` is at or under the prefix
/// (its subtree may hold prefix-matching keys — a string-prefix match, like
/// S3/local), or if `dir` is a **path-ancestor** of the prefix (we must pass
/// through it to reach the target). The ancestor test is boundary-aware (`"{dir}/"`),
/// so listing under `docs/report` doesn't needlessly walk a sibling `doc/` whose
/// name is merely a string-prefix of the target — saving a spurious recursive
/// PROPFIND over that sibling's whole subtree.
fn should_descend(dir: &str, prefix: &str) -> bool {
    dir.starts_with(prefix) || prefix.starts_with(&format!("{dir}/"))
}

fn href_to_key(href: &str, base_path: &str) -> String {
    let path = if href.contains("://") {
        Url::parse(href)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| href.to_string())
    } else {
        href.to_string()
    };
    let rel = path
        .strip_prefix(base_path)
        .unwrap_or_else(|| path.trim_start_matches('/'));
    rel.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect::<Vec<_>>()
        .join("/")
}

/// Minimal percent-decoder (no extra dependency) for an `href` path.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse a `getlastmodified` timestamp as UTC. Tries RFC 1123 (the WebDAV norm,
/// e.g. `Wed, 17 Jun 2026 02:00:00 GMT`), then RFC 2822 (numeric offsets) and
/// RFC 3339 / ISO-8601 (Nextcloud and others emit these) as fallbacks.
fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT")
        .ok()
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .or_else(|| {
            DateTime::parse_from_rfc2822(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
        .or_else(|| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn href_strips_base_and_decodes() {
        assert_eq!(href_to_key("/docs/readme.txt", "/"), "docs/readme.txt");
        assert_eq!(href_to_key("/dav/docs/a%20b.txt", "/dav/"), "docs/a b.txt");
        assert_eq!(href_to_key("http://h:8080/docs/x.md", "/"), "docs/x.md");
        // A trailing-slash collection href yields a key without it.
        assert_eq!(href_to_key("/docs/", "/"), "docs");
    }

    #[test]
    fn should_descend_prunes_siblings_at_path_boundaries() {
        // Ancestor of the prefix → descend toward the target.
        assert!(should_descend("docs", "docs/report"));
        // At/under the prefix (string-prefix match, like S3/local) → descend.
        assert!(should_descend("docs", "docs"));
        assert!(should_descend("docs/reports", "docs/report"));
        assert!(should_descend("docs", "doc")); // "docs/x" matches prefix "doc"
        assert!(should_descend("anything", "")); // empty prefix lists everything
                                                 // A sibling whose name is only a *string* prefix of the target is NOT walked
                                                 // (the boundary fix): listing `docs/report` must not recurse into `doc/`.
        assert!(!should_descend("doc", "docs/report"));
        // An unrelated subtree under a shared ancestor is pruned too.
        assert!(!should_descend("docs/other", "docs/report"));
    }

    #[test]
    fn parses_multistatus_marks_dirs_and_reads_2xx_props_only() {
        // The file's `getetag` is in a SECOND, separate 200 propstat, and a 404
        // propstat carries an empty placeholder `getcontentlength` FIRST — a naive
        // "first prop" reader would report size 0 and miss the etag.
        let xml = r#"<?xml version="1.0"?>
        <D:multistatus xmlns:D="DAV:">
          <D:response><D:href>/notes/</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype></D:prop>
            <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
          <D:response><D:href>/notes/hello.md</D:href>
            <D:propstat><D:prop><D:getcontentlength></D:getcontentlength></D:prop>
              <D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
            <D:propstat><D:prop><D:getcontentlength>4</D:getcontentlength>
              <D:getcontenttype>text/markdown</D:getcontenttype>
              <D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
            <D:propstat><D:prop><D:getetag>"abc"</D:getetag></D:prop>
              <D:status>HTTP/1.1 200 OK</D:status></D:propstat>
          </D:response>
        </D:multistatus>"#;
        let items = parse_multistatus(xml, "/").unwrap();
        assert_eq!(items.len(), 2, "both the subdir and the file are returned");
        let dir = items.iter().find(|e| e.is_dir).expect("the subdir entry");
        assert_eq!(dir.meta.key, "notes", "the collection is marked is_dir");
        let file = items.iter().find(|e| !e.is_dir).expect("the file entry");
        assert_eq!(file.meta.key, "notes/hello.md");
        assert_eq!(
            file.meta.size, 4,
            "size read from the 200 block, not the empty 404 one"
        );
        assert_eq!(
            file.meta.etag.as_deref(),
            Some("abc"),
            "etag read from a second 200 block"
        );
        assert_eq!(file.meta.content_type.as_deref(), Some("text/markdown"));
    }

    #[test]
    fn validate_key_rejects_traversal_and_empty() {
        assert!(validate_key("a/b.txt").is_ok());
        assert!(validate_key("ws//a").is_ok()); // collapsed double-slash, like local
        for bad in ["", ".", "..", "a/../b", "a/./b", "../escape"] {
            assert!(validate_key(bad).is_err(), "key `{bad}` must be rejected");
        }
    }

    #[test]
    fn http_date_falls_back_to_rfc3339() {
        assert!(parse_http_date("Wed, 17 Jun 2026 02:00:00 GMT").is_some());
        assert!(parse_http_date("2026-06-17T02:00:00Z").is_some());
        assert!(parse_http_date("not a date").is_none());
    }
}
