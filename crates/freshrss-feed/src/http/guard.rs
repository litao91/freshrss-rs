//! Which addresses the fetcher is allowed to connect to.
//!
//! A feed URL comes from a user, and so does every link on a page that discovery follows. Without a
//! guard, "add this feed" is a primitive for making the server send requests on the attacker's
//! behalf: `http://169.254.169.254/latest/meta-data/` reads a cloud instance's metadata,
//! `http://192.168.1.1/admin` reaches the operator's router, and `http://127.0.0.1:9200/_search`
//! reaches anything else listening on the host. Whatever comes back is then shown in the add-feed
//! dialog, so the response is read out loud — which is why the check has to be on the *address
//! connected to* and not on the URL string.
//!
//! Two consequences shape this module:
//!
//! - The check belongs in name resolution, not in URL parsing. A hostname that resolves to
//!   `127.0.0.1` is indistinguishable from one that resolves to a public address until it is
//!   resolved, and an attacker controls the DNS for any name they like. Checking the string first
//!   and connecting later also loses to a name that answers differently the second time.
//! - A name that resolves to several addresses is filtered, not rejected. Dropping the private ones
//!   and connecting to a remaining public one is both safe and non-breaking, whereas failing the
//!   whole name would break a legitimately multi-homed host for no security benefit.
//!
//! Hosts on a private network that *should* be fetchable — an internal wiki's feed, a NAS — go on
//! [`Guard`]'s allowlist rather than turning the guard off.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// True for an address that is reachable on the public internet.
///
/// The complement is everything RFC 1122, RFC 1918, RFC 4193, RFC 4291 and RFC 6598 reserve:
/// unspecified, loopback, link-local, private, shared, multicast, broadcast, and the transition
/// mechanisms that carry an IPv4 address inside an IPv6 one.
pub fn is_remote(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_remote_v4(v4),
        IpAddr::V6(v6) => is_remote_v6(v6),
    }
}

fn is_remote_v4(ip: &Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !matches!(
        (a, b),
        // 0.0.0.0/8, "this network"
        (0, _)
        // 10.0.0.0/8
        | (10, _)
        // 127.0.0.0/8 loopback
        | (127, _)
        // 169.254.0.0/16 link-local, which is where cloud metadata endpoints live
        | (169, 254)
        // 172.16.0.0/12
        | (172, 16..=31)
        // 192.168.0.0/16
        | (192, 168)
        // 100.64.0.0/10 carrier-grade NAT: not public, and not the operator's LAN either
        | (100, 64..=127)
        // 224.0.0.0/4 multicast, and 240.0.0.0/4 reserved (which includes the broadcast address)
        | (224..=255, _)
    )
}

fn is_remote_v6(ip: &Ipv6Addr) -> bool {
    let segments = ip.segments();
    // ::/128 unspecified and ::1/128 loopback
    if ip == &Ipv6Addr::UNSPECIFIED || ip == &Ipv6Addr::LOCALHOST {
        return false;
    }
    // fc00::/7 unique-local, the IPv6 analogue of RFC 1918. Both halves: fc00::/8 is reserved and
    // fd00::/8 is the randomly-assigned space that is actually in use.
    if segments[0] & 0xfe00 == 0xfc00 {
        return false;
    }
    // fe80::/10 link-local — which spans fe80 through febf, not just fe80
    if segments[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // fec0::/10 site-local: deprecated by RFC 3879 and never globally routable
    if segments[0] & 0xffc0 == 0xfec0 {
        return false;
    }
    // ff00::/8 multicast
    if segments[0] & 0xff00 == 0xff00 {
        return false;
    }
    // ::ffff:0:0/96 IPv4-mapped, and ::/96 IPv4-compatible before it. Both are an IPv4 address in
    // IPv6 clothing, so the IPv4 rules apply — judging them as IPv6 would let `::ffff:127.0.0.1`
    // through, and that is the form an attacker reaches for first.
    if let Some(v4) = embedded_v4(ip) {
        return is_remote_v4(&v4);
    }
    // 2002::/16 6to4 carries the tunnel's IPv4 in its second and third segments, so a 6to4 address
    // is a route to whatever that IPv4 is.
    if segments[0] == 0x2002 {
        return is_remote_v4(&Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        ));
    }
    // 2001:0000::/32 Teredo. The client address it embeds is complemented and the relay is chosen
    // by the prefix, so rather than decode either, refuse the range: it is deprecated, essentially
    // unused for hosting, and the only reason to point a feed reader at it is to get past a check.
    if segments[0] == 0x2001 && segments[1] == 0 {
        return false;
    }
    true
}

/// The IPv4 address an IPv6 one carries, in the mapped (`::ffff:a.b.c.d`) or the deprecated
/// compatible (`::a.b.c.d`) form.
fn embedded_v4(ip: &Ipv6Addr) -> Option<Ipv4Addr> {
    let [s0, s1, s2, s3, s4, s5, s6, s7] = ip.segments();
    if s0 | s1 | s2 | s3 | s4 != 0 || (s5 != 0 && s5 != 0xffff) {
        return None;
    }
    Some(Ipv4Addr::new(
        (s6 >> 8) as u8,
        s6 as u8,
        (s7 >> 8) as u8,
        s7 as u8,
    ))
}

/// One allowlist entry that names a network rather than a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Net {
    base: IpAddr,
    prefix: u8,
}

impl Net {
    fn parse(text: &str) -> Option<Net> {
        let (addr, prefix) = text.split_once('/')?;
        let base: IpAddr = addr.parse().ok()?;
        let prefix: u8 = prefix.parse().ok()?;
        // A /33 on an IPv4 address is a typo, not a very wide network; accepting it would silently
        // exempt nothing, or everything, depending on how the shift is written.
        if u32::from(prefix) > if base.is_ipv4() { 32 } else { 128 } {
            return None;
        }
        Some(Net { base, prefix })
    }

    fn contains(&self, ip: &IpAddr) -> bool {
        if self.prefix == 0 {
            return true;
        }
        // An IPv4-mapped or -compatible address is the address it carries, so a v4 network entry has
        // to match it: a host can resolve to `::ffff:192.168.1.5` just as easily as to `192.168.1.5`,
        // and exempting one form but not the other would be a hole shaped like a resolver.
        let target = match (self.base, ip) {
            (IpAddr::V4(_), IpAddr::V6(v6)) => match embedded_v4(v6) {
                Some(v4) => IpAddr::V4(v4),
                None => *ip,
            },
            _ => *ip,
        };
        match (self.base, target) {
            (IpAddr::V4(base), IpAddr::V4(other)) => {
                let shift = 32 - u32::from(self.prefix);
                u32::from(base) >> shift == u32::from(other) >> shift
            }
            (IpAddr::V6(base), IpAddr::V6(other)) => {
                let shift = 128 - u32::from(self.prefix);
                u128::from(base) >> shift == u128::from(other) >> shift
            }
            // A v6 network entry says nothing about a v4 address. The reverse is handled above.
            _ => false,
        }
    }
}

/// The network policy one fetcher applies.
///
/// Cheap to clone, so a request can borrow it and a resolver can own it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Guard {
    /// Whether non-remote addresses are refused at all. Off makes the guard a no-op, which is what
    /// a reader confined to a LAN needs and what the CLI's one-off `parse` command wants.
    pub block_private: bool,
    hosts: HashSet<String>,
    nets: Vec<Net>,
}

impl Guard {
    /// A guard, with `allowlist` exempt from it.
    ///
    /// Entries are hostnames (case-insensitive), single IP addresses, or CIDR networks. An entry
    /// that is none of those is ignored rather than fatal: it is a config typo, and refusing every
    /// fetch because of one bad line is worse than the line doing nothing. [`Guard::rejected`]
    /// reports what was dropped so the caller can log it once at startup.
    pub fn new(block_private: bool, allowlist: &[String]) -> Self {
        let mut guard = Guard {
            block_private,
            ..Guard::default()
        };
        for entry in allowlist {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(net) = Net::parse(trimmed) {
                guard.nets.push(net);
                continue;
            }
            // A bare address is kept as a host entry: a URL spells its host as a literal, and the
            // resolver is never consulted for one, so an address exemption has to match by name.
            if trimmed.parse::<IpAddr>().is_ok() || is_plausible_host(trimmed) {
                guard.hosts.insert(trimmed.to_ascii_lowercase());
            }
        }
        guard
    }

    /// Allowlist entries that could not be understood, for a startup warning.
    pub fn rejected(allowlist: &[String]) -> Vec<String> {
        allowlist
            .iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .filter(|entry| {
                Net::parse(entry).is_none()
                    && entry.parse::<IpAddr>().is_err()
                    && !is_plausible_host(entry)
            })
            .map(str::to_owned)
            .collect()
    }

    /// True when this name is exempt outright, so it need not be resolved and filtered.
    ///
    /// Also how a URL whose host is written as an IP literal is exempted: the resolver is never
    /// consulted for a literal, so the literal has to be checked against the same set.
    pub fn allows_host(&self, host: &str) -> bool {
        self.hosts.contains(&host.to_ascii_lowercase())
    }

    /// True when connecting to `ip` is permitted.
    pub fn allows_addr(&self, ip: &IpAddr) -> bool {
        if !self.block_private {
            return true;
        }
        is_remote(ip) || self.nets.iter().any(|net| net.contains(ip))
    }

    /// The addresses of a resolved name that may be connected to, in the order DNS returned them.
    ///
    /// Empty means the name resolves to nothing fetchable, which the caller turns into
    /// [`FeedError::NonRemote`](crate::FeedError::NonRemote).
    pub fn permitted(&self, host: &str, addrs: impl Iterator<Item = IpAddr>) -> Vec<IpAddr> {
        if self.allows_host(host) {
            return addrs.collect();
        }
        addrs.filter(|ip| self.allows_addr(ip)).collect()
    }
}

/// Whether a string could be a hostname.
///
/// Deliberately permissive — punycode, underscores and single-label internal names are all the
/// operator's business, and rejecting one would silently drop an exemption they meant. The one
/// thing it insists on is a letter, so that a mistyped address (`999.1.1.1`, `10.0.0/8`) is
/// reported instead of being stored as a hostname that can never match.
fn is_plausible_host(entry: &str) -> bool {
    entry.len() <= 253
        && entry.bytes().any(|b| b.is_ascii_alphabetic())
        && !entry.contains(' ')
        && entry
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(text: &str) {
        assert!(is_remote(&text.parse().unwrap()), "{text} should be remote");
    }

    fn blocked(text: &str) {
        assert!(
            !is_remote(&text.parse().unwrap()),
            "{text} should be blocked"
        );
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("a valid address")
    }

    fn guard(allowlist: &[&str]) -> Guard {
        Guard::new(
            true,
            &allowlist
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn public_v4_addresses_are_remote() {
        for text in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "11.0.0.1",
            "172.32.0.1",
            "192.169.0.1",
            "100.128.0.1",
            "223.255.255.255",
        ] {
            remote(text);
        }
    }

    #[test]
    fn reserved_v4_addresses_are_blocked() {
        for text in [
            "0.0.0.0",
            "0.255.255.255",
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "10.255.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            blocked(text);
        }
    }

    #[test]
    fn the_widely_mistaken_v4_boundaries_are_exact() {
        // 172.16/12, not 172/8: 172.15 and 172.32 are public.
        remote("172.15.255.255");
        blocked("172.16.0.0");
        blocked("172.31.255.255");
        remote("172.32.0.0");
        // 100.64/10, not 100.64/8.
        remote("100.63.255.255");
        blocked("100.64.0.0");
        blocked("100.127.255.255");
        remote("100.128.0.0");
        // 169.254/16 only, not all of 169/8.
        remote("169.253.1.1");
        blocked("169.254.0.0");
        remote("169.255.0.0");
    }

    #[test]
    fn public_v6_addresses_are_remote() {
        for text in [
            "2606:2800:220:1:248:1893:25c8:1946",
            "2001:4860:4860::8888",
            "2a00:1450:4009:800::200e",
            "fbff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ] {
            remote(text);
        }
    }

    #[test]
    fn reserved_v6_addresses_are_blocked() {
        for text in [
            "::",
            "::1",
            "fc00::1",
            "fd00::1",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe80::1",
            "febf:ffff::1",
            "ff00::1",
            "ffff::1",
            "2001::1",
            "fec0::1",
        ] {
            blocked(text);
        }
        // fe80::/10 spans fe80 through febf, fc00::/7 spans fc00 through fdff, and fec0::/10 spans
        // fec0 through feff. Each boundary is one bit away from an address that *is* public.
        remote("fe00::1");
        remote("fbff::1");
        remote("f800::1");
    }

    #[test]
    fn an_ipv4_address_in_ipv6_clothing_is_checked_as_ipv4() {
        blocked("::ffff:127.0.0.1");
        blocked("::ffff:169.254.169.254");
        blocked("::ffff:10.0.0.1");
        remote("::ffff:8.8.8.8");
        // The deprecated IPv4-compatible form, which `Ipv6Addr::to_ipv4_mapped` does not cover.
        blocked("::127.0.0.1");
        blocked("::10.1.2.3");
        remote("::8.8.8.8");
    }

    #[test]
    fn a_6to4_address_is_judged_by_the_ipv4_it_carries() {
        blocked("2002:7f00:1::1");
        blocked("2002:a9fe:a9fe::1");
        remote("2002:808:808::1");
    }

    #[test]
    fn malformed_allowlist_entries_are_reported_rather_than_fatal() {
        let entries = [
            "nas.lan".to_owned(),
            "10.0.0.0/8".to_owned(),
            "192.168.1.50".to_owned(),
            "not a host".to_owned(),
            "10.0.0.0/33".to_owned(),
            "999.1.1.1".to_owned(),
        ];
        assert_eq!(
            Guard::rejected(&entries),
            vec![
                "not a host".to_owned(),
                "10.0.0.0/33".to_owned(),
                "999.1.1.1".to_owned()
            ]
        );
        // And the good ones still took effect.
        let g = Guard::new(true, &entries);
        assert!(g.allows_host("nas.lan"));
        assert!(
            g.allows_host("192.168.1.50"),
            "a bare address is a host entry, because that is how a URL spells it"
        );
        assert!(g.allows_addr(&ip("10.1.2.3")));
        assert!(
            !g.allows_addr(&ip("172.20.0.1")),
            "10/8 does not exempt the rest of RFC 1918"
        );
    }

    #[test]
    fn a_disabled_guard_permits_everything() {
        let off = Guard::new(false, &[]);
        assert!(off.allows_addr(&ip("127.0.0.1")));
        assert!(off.allows_addr(&ip("169.254.169.254")));
        assert_eq!(
            off.permitted("localhost", [ip("127.0.0.1")].into_iter()),
            vec![ip("127.0.0.1")]
        );
    }

    #[test]
    fn an_allowlisted_host_is_exempt_whatever_it_resolves_to() {
        let g = guard(&["nas.lan", "Intranet.Example"]);
        assert!(g.allows_host("nas.lan"));
        assert!(g.allows_host("NAS.LAN"), "hostnames are case-insensitive");
        assert!(g.allows_host("intranet.example"));
        assert!(!g.allows_host("other.lan"));
        assert!(!g.allows_host("nas.lan.example"));

        let addrs = [ip("192.168.1.5"), ip("127.0.0.1")];
        assert_eq!(
            g.permitted("nas.lan", addrs.iter().copied()),
            addrs.to_vec()
        );
    }

    #[test]
    fn an_allowlisted_network_is_exempt() {
        let g = guard(&["192.168.0.0/16", "10.0.0.0/8", "fd12:3456::/32"]);
        assert!(g.allows_addr(&ip("192.168.7.1")));
        assert!(g.allows_addr(&ip("10.255.0.1")));
        assert!(g.allows_addr(&ip("fd12:3456:7890::1")));
        assert!(!g.allows_addr(&ip("127.0.0.1")));
        assert!(!g.allows_addr(&ip("fd12:3457::1")));
        // A private address no entry names is still refused: an allowlist widens what is allowed, it
        // does not turn the guard off. (A *public* address is allowed by `is_remote` and so cannot
        // show this — hence a private one.)
        assert!(!g.allows_addr(&ip("172.20.0.1")));
    }

    #[test]
    fn a_v4_network_entry_covers_the_mapped_form_too() {
        let g = guard(&["192.168.0.0/16"]);
        assert!(g.allows_addr(&ip("192.168.7.1")));
        assert!(
            g.allows_addr(&ip("::ffff:192.168.7.1")),
            "a resolver may hand back the mapped form of the same address"
        );
        assert!(!g.allows_addr(&ip("::ffff:10.7.0.1")));
    }

    #[test]
    fn a_single_address_may_be_allowlisted_as_a_network() {
        let g = guard(&["127.0.0.1/32", "::1/128"]);
        assert!(g.allows_addr(&ip("127.0.0.1")));
        assert!(g.allows_addr(&ip("::1")));
        assert!(!g.allows_addr(&ip("127.0.0.2")));
    }

    #[test]
    fn an_unlisted_host_is_filtered_to_its_remote_addresses() {
        let g = guard(&[]);
        let addrs = [ip("169.254.1.1"), ip("93.184.216.34"), ip("10.0.0.1")];
        assert_eq!(
            g.permitted("example.com", addrs.into_iter()),
            vec![ip("93.184.216.34")]
        );
        assert!(
            g.permitted("localhost", [ip("127.0.0.1")].into_iter())
                .is_empty(),
            "nothing left to connect to"
        );
    }
}
