//! Fetch safety policy — the SSRF / egress guard (SOUL §13, §19).
//!
//! A server-side "fetch this URL" tool is a classic SSRF vector: left open, the
//! LLM (or a prompt-injected page) can make catalerum reach the cloud metadata
//! endpoint, an internal admin panel, or `localhost`. [`FetchPolicy`] is the
//! deny-by-default gate every backend runs before it connects: only `http(s)`,
//! and — unless explicitly allowed — never a private, loopback, link-local, or
//! otherwise non-public address. Hostnames are also resolved and re-checked so a
//! public name pointing at `127.0.0.1` is refused too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use catalerum_core::error::{Error, Result};
use url::{Host, Url};

/// The egress policy a [`super::WebFetcher`] enforces before connecting.
#[derive(Clone, Debug)]
pub struct FetchPolicy {
    /// Allow reaching private / loopback / link-local addresses. Off by default
    /// (a protected scope, SOUL §19); turn on only for trusted self-hosted
    /// targets behind the firewall.
    pub allow_private_hosts: bool,
    /// Cap on bytes read from a response body (defends against huge pages).
    pub max_bytes: u64,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            allow_private_hosts: false,
            // 5 MiB of HTML is already an enormous page; plenty for reading.
            max_bytes: 5 * 1024 * 1024,
        }
    }
}

impl FetchPolicy {
    /// Parse and validate a request URL: must be absolute `http(s)` with a host,
    /// and (unless `allow_private_hosts`) not an obviously-private literal IP or
    /// loopback name. Returns the parsed [`Url`]. DNS names are re-checked after
    /// resolution by [`Self::guard_resolved`].
    pub fn validate(&self, raw: &str) -> Result<Url> {
        let url = Url::parse(raw).map_err(|e| Error::invalid(format!("invalid url: {e}")))?;
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(Error::invalid(format!("unsupported url scheme `{other}`"))),
        }
        let host = url
            .host()
            .ok_or_else(|| Error::invalid("url has no host"))?;
        if self.allow_private_hosts {
            return Ok(url);
        }
        match host {
            Host::Ipv4(ip) => {
                if ip_is_blocked(IpAddr::V4(ip)) {
                    return Err(blocked(&ip.to_string()));
                }
            }
            Host::Ipv6(ip) => {
                if ip_is_blocked(IpAddr::V6(ip)) {
                    return Err(blocked(&ip.to_string()));
                }
            }
            Host::Domain(name) => {
                if host_name_is_local(name) {
                    return Err(blocked(name));
                }
            }
        }
        Ok(url)
    }

    /// Resolve `url`'s host and reject if *any* resolved address is non-public
    /// (defeats a public name that points at an internal IP). No-op when private
    /// hosts are allowed or the host is already an IP literal (checked in
    /// [`Self::validate`]).
    pub async fn guard_resolved(&self, url: &Url) -> Result<()> {
        if self.allow_private_hosts {
            return Ok(());
        }
        let Some(Host::Domain(name)) = url.host() else {
            return Ok(());
        };
        let port = url.port_or_known_default().unwrap_or(80);
        let addrs = tokio::net::lookup_host((name, port))
            .await
            .map_err(|e| Error::provider(format!("dns lookup for `{name}` failed: {e}")))?;
        for addr in addrs {
            if ip_is_blocked(addr.ip()) {
                return Err(blocked(&format!("{name} → {}", addr.ip())));
            }
        }
        Ok(())
    }
}

fn blocked(target: &str) -> Error {
    Error::unauthorized(format!(
        "fetching `{target}` is blocked (private/loopback address; needs an explicit allow)"
    ))
}

/// Truncate a string to at most `max` bytes without splitting a `char`. Used by
/// backends that receive content whole (Firecrawl, browser) to honour
/// [`FetchPolicy::max_bytes`] (SOUL §27).
#[must_use]
#[cfg_attr(not(any(feature = "firecrawl", feature = "browser")), allow(dead_code))]
pub(crate) fn cap_str(s: &str, max: u64) -> &str {
    let max = max as usize;
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// True for a hostname that names the local machine without being an IP literal.
fn host_name_is_local(name: &str) -> bool {
    let n = name.trim_end_matches('.').to_ascii_lowercase();
    n == "localhost"
        || n.ends_with(".localhost")
        || n.ends_with(".local")
        || n.ends_with(".internal")
}

/// The first resolved address that must not be connected to (per [`ip_is_blocked`]),
/// or `None` when all are allowed. Always `None` when `allow_private` is set (the
/// trusted self-hosted opt-in). Used by the **connect-time** DNS guard so reqwest
/// only ever connects to a vetted address — closing the rebind window between the
/// pre-flight [`FetchPolicy::guard_resolved`] and reqwest's own resolution. Any one
/// blocked address among the results refuses the whole host (rebind / multi-record
/// defence), matching `guard_resolved`.
#[must_use]
pub fn first_blocked_addr(addrs: &[SocketAddr], allow_private: bool) -> Option<IpAddr> {
    if allow_private {
        return None;
    }
    addrs
        .iter()
        .map(SocketAddr::ip)
        .find(|ip| ip_is_blocked(*ip))
}

/// True when an IP must not be reached from a server-side fetch.
#[must_use]
pub fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_blocked(v4),
        IpAddr::V6(v6) => v6_is_blocked(v6),
    }
}

fn v4_is_blocked(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    ip.is_loopback()            // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254/16
        || ip.is_unspecified()  // 0.0.0.0
        || ip.is_broadcast()    // 255.255.255.255
        || ip.is_multicast()
        || a == 0               // 0.0.0.0/8
        || (a == 100 && (64..=127).contains(&b)) // 100.64/10 CGNAT
}

fn v6_is_blocked(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    // Any embedded IPv4 is re-checked against the v4 rules so a v6 literal can't
    // smuggle a private/metadata target past as "plain" v6:
    //  - IPv4-mapped `::ffff:a.b.c.d` and the deprecated IPv4-compatible `::a.b.c.d`
    //    (both decoded by `to_ipv4`; `::`/`::1` are already handled above);
    //  - the NAT64 well-known prefix `64:ff9b::/96` (RFC 6052), which `to_ipv4`
    //    does **not** decode — on a NAT64 network `64:ff9b::169.254.169.254` would
    //    translate to a connection to the v4 metadata endpoint;
    //  - the 6to4 prefix `2002::/16` (RFC 3056), which carries the v4 in the first
    //    32 bits after the prefix — on a host with 6to4 connectivity `2002:a9fe:a9fe::`
    //    routes (via a 6to4 relay) to `169.254.169.254`. The same embedded-v4
    //    smuggling class as NAT64, decoded by `sixto4_embedded_v4`.
    //  - the Teredo prefix `2001:0000::/32` (RFC 4380), whose last 32 bits are the
    //    client IPv4 obfuscated by XOR `0xffffffff` — on a host with Teredo
    //    connectivity `2001:0:0:0:0:0:5601:5601` decodes to `169.254.169.254`. Same
    //    class again, decoded by `teredo_embedded_v4`.
    // A NAT64/6to4/Teredo address embedding a *public* v4 stays allowed (the
    // embedded address is what counts). The decoders return None for real public
    // v6, which falls through to the prefix checks below.
    if let Some(v4) = ip
        .to_ipv4()
        .or_else(|| nat64_embedded_v4(ip))
        .or_else(|| sixto4_embedded_v4(ip))
        .or_else(|| teredo_embedded_v4(ip))
    {
        return v4_is_blocked(v4);
    }
    let seg0 = ip.segments()[0];
    (seg0 & 0xfe00) == 0xfc00   // fc00::/7 unique-local
        || (seg0 & 0xffc0) == 0xfe80 // fe80::/10 link-local
}

/// The IPv4 address embedded in the NAT64 well-known prefix `64:ff9b::/96`
/// (RFC 6052), or `None` if `ip` is not in that prefix. The trailing 32 bits hold
/// the IPv4 address; the upper 96 bits are the fixed prefix.
fn nat64_embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let in_prefix =
        s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0;
    in_prefix.then(|| {
        Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        )
    })
}

/// The IPv4 address embedded in a 6to4 address (`2002::/16`, RFC 3056), or `None`
/// if `ip` is not 6to4. The v4 occupies the 32 bits immediately after the `2002`
/// prefix (segments `[1]` and `[2]`); the remaining bits are the 6to4 site's own
/// subnet/interface and don't affect which v4 the packet is relayed to.
fn sixto4_embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    (s[0] == 0x2002).then(|| {
        Ipv4Addr::new(
            (s[1] >> 8) as u8,
            (s[1] & 0xff) as u8,
            (s[2] >> 8) as u8,
            (s[2] & 0xff) as u8,
        )
    })
}

/// The client IPv4 embedded in a Teredo address (`2001:0000::/32`, RFC 4380), or
/// `None` if `ip` is not Teredo. The last 32 bits (segments `[6]`/`[7]`) hold the
/// client's IPv4 **obfuscated by XOR with `0xffffffff`**; the upper bits carry the
/// Teredo server v4, flags, and obscured UDP port (not the connection peer). On a
/// host with Teredo connectivity this client v4 is the actual endpoint reached, so
/// it's screened with the same v4 rules as NAT64/6to4.
fn teredo_embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    (s[0] == 0x2001 && s[1] == 0x0000).then(|| {
        let hi = s[6] ^ 0xffff;
        let lo = s[7] ^ 0xffff;
        Ipv4Addr::new(
            (hi >> 8) as u8,
            (hi & 0xff) as u8,
            (lo >> 8) as u8,
            (lo & 0xff) as u8,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_loopback_ipv4() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.5.4",
            "169.254.1.1",
            "0.0.0.0",
            "100.64.0.1",
        ] {
            assert!(ip_is_blocked(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            assert!(
                !ip_is_blocked(ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
    }

    #[test]
    fn blocks_ipv6_local() {
        assert!(ip_is_blocked("::1".parse().unwrap()));
        assert!(ip_is_blocked("fe80::1".parse().unwrap()));
        assert!(ip_is_blocked("fc00::1".parse().unwrap()));
        // IPv4-mapped and the deprecated IPv4-compatible form both decode to the
        // embedded v4 and are blocked (e.g. the cloud-metadata address).
        assert!(ip_is_blocked("::ffff:127.0.0.1".parse().unwrap()));
        assert!(ip_is_blocked("::ffff:169.254.169.254".parse().unwrap()));
        assert!(ip_is_blocked("::169.254.169.254".parse().unwrap()));
        assert!(ip_is_blocked("::127.0.0.1".parse().unwrap()));
        // NAT64 well-known prefix (64:ff9b::/96) embedding a blocked v4 — `to_ipv4`
        // does not decode it, so without the NAT64 check it would slip through and,
        // on a NAT64 network, reach the v4 target (e.g. cloud metadata).
        assert!(ip_is_blocked("64:ff9b::169.254.169.254".parse().unwrap()));
        assert!(ip_is_blocked("64:ff9b::127.0.0.1".parse().unwrap()));
        assert!(ip_is_blocked("64:ff9b::10.0.0.1".parse().unwrap()));
        // A real public IPv6 is allowed (not in ::/96, so no embedded-v4 decode)…
        assert!(!ip_is_blocked("2606:4700:4700::1111".parse().unwrap()));
        // …and NAT64 to a *public* v4 stays allowed (only the embedded addr matters).
        assert!(!ip_is_blocked("64:ff9b::8.8.8.8".parse().unwrap()));
        // 6to4 (2002::/16, RFC 3056) carries the v4 in the 32 bits after the prefix;
        // `to_ipv4` doesn't decode it, so without `sixto4_embedded_v4` a 6to4 literal
        // would slip past `validate` and, on a 6to4 host, reach the embedded v4.
        assert!(ip_is_blocked("2002:a9fe:a9fe::1".parse().unwrap())); // → 169.254.169.254 (metadata)
        assert!(ip_is_blocked("2002:7f00:0001::".parse().unwrap())); // → 127.0.0.1
        assert!(ip_is_blocked("2002:0a00:0001::1".parse().unwrap())); // → 10.0.0.1
                                                                      // 6to4 embedding a *public* v4 stays allowed (only the embedded addr matters).
        assert!(!ip_is_blocked("2002:0808:0808::".parse().unwrap())); // → 8.8.8.8
                                                                      // Teredo (2001:0000::/32, RFC 4380) carries the client v4 in the last 32 bits
                                                                      // XOR'd with 0xffffffff; `to_ipv4` doesn't decode it, so without
                                                                      // `teredo_embedded_v4` a Teredo literal would slip past `validate` and, on a
                                                                      // Teredo host, reach the embedded v4.
        assert!(ip_is_blocked("2001:0:0:0:0:0:5601:5601".parse().unwrap())); // → 169.254.169.254 (metadata)
        assert!(ip_is_blocked("2001:0:0:0:0:0:80ff:fffe".parse().unwrap())); // → 127.0.0.1
        assert!(ip_is_blocked("2001:0:0:0:0:0:f5ff:fffe".parse().unwrap())); // → 10.0.0.1
                                                                             // Teredo embedding a *public* client v4 stays allowed.
        assert!(!ip_is_blocked("2001:0:0:0:0:0:f7f7:f7f7".parse().unwrap())); // → 8.8.8.8
                                                                              // A real public 2001:: address (db8 documentation block) is not Teredo
                                                                              // (segment[1] != 0) and stays allowed.
        assert!(!ip_is_blocked("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn first_blocked_addr_screens_resolved_addresses() {
        let pub_v4: SocketAddr = "93.184.216.34:0".parse().unwrap();
        let loop_v4: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let priv_v4: SocketAddr = "10.0.0.1:0".parse().unwrap();
        let nat64_meta: SocketAddr = "[64:ff9b::169.254.169.254]:0".parse().unwrap();
        // All public → nothing blocked.
        assert!(first_blocked_addr(&[pub_v4], false).is_none());
        // One blocked address among the results refuses the host (rebind defence).
        assert_eq!(
            first_blocked_addr(&[pub_v4, priv_v4], false),
            Some(priv_v4.ip())
        );
        assert_eq!(first_blocked_addr(&[loop_v4], false), Some(loop_v4.ip()));
        // The v6 embedded-IPv4 rules apply here too (NAT64 metadata).
        assert_eq!(
            first_blocked_addr(&[nat64_meta], false),
            Some(nat64_meta.ip())
        );
        // The trusted opt-in bypasses screening; empty input has nothing to block.
        assert!(first_blocked_addr(&[priv_v4], true).is_none());
        assert!(first_blocked_addr(&[], false).is_none());
    }

    #[test]
    fn validate_rejects_schemes_and_locals() {
        let p = FetchPolicy::default();
        assert!(p.validate("ftp://example.com").is_err());
        assert!(p.validate("file:///etc/passwd").is_err());
        assert!(p.validate("http://localhost/admin").is_err());
        assert!(p.validate("http://127.0.0.1:8787/").is_err());
        assert!(p.validate("http://foo.internal/").is_err());
        assert!(p.validate("https://example.com/page").is_ok());
    }

    #[test]
    fn cap_str_respects_char_boundaries() {
        assert_eq!(cap_str("hello", 100), "hello");
        assert_eq!(cap_str("hello", 3), "hel");
        // "é" is 2 bytes; a 1-byte cap must not split it.
        assert_eq!(cap_str("é", 1), "");
        assert_eq!(cap_str("aé", 2), "a");
        assert_eq!(cap_str("世界", 4), "世"); // 3 bytes each → cap 4 keeps one
    }

    #[test]
    fn allow_private_opts_in() {
        let p = FetchPolicy {
            allow_private_hosts: true,
            ..FetchPolicy::default()
        };
        assert!(p.validate("http://localhost:8080/").is_ok());
        assert!(p.validate("http://10.0.0.5/").is_ok());
    }
}
