//! IPv6 link-local neighbor discovery. Link-local addresses (`fe80::/10`)
//! self-assign on any live Ethernet link with zero configuration on either
//! end — no DHCP, no subnet match needed. This is what saves the day when a
//! site's IPv4 network has been reconfigured out from under you: it works
//! even when nothing about the IPv4 side does. Windows also runs IPv6
//! neighbor discovery in the background more or less automatically, so the
//! local neighbor cache often already has the far end's address the moment
//! a cable is plugged in.
//!
//! Parsing of `netsh interface ipv6 show neighbors` output is kept pure/
//! testable; `discover_neighbors` does the real process spawning.

use crate::winproc;

#[derive(Debug, Clone, PartialEq)]
pub struct Ipv6Neighbor {
    /// Full Windows-usable literal, e.g. "fe80::fc9c:a7ff:fec8:dc64%20" —
    /// link-local addresses always need the `%<zone>` suffix on Windows for
    /// ping/tracert/ssh/URLs, so it's baked in here rather than tacked on
    /// by every caller.
    pub address_with_zone: String,
    pub mac: Option<String>,
}

/// Runs a best-effort "wake up the neighbor cache" nudge (a single ping to
/// the all-nodes multicast address, result entirely ignored — see the
/// module-level note below on why), then reads the real neighbor table.
pub fn discover_neighbors(iface_idx: u32, iface_name: &str) -> Vec<Ipv6Neighbor> {
    // Windows' ping.exe does not reliably surface individual responders to a
    // multicast destination (observed: "Request timed out" even when the
    // neighbor table already had a live entry for that link) — so this is
    // purely a best-effort nudge to encourage fresh discovery traffic, not a
    // data source. Never parse its output.
    let _ = winproc::command("ping")
        .args(["-6", "-n", "1", "-w", "500", &format!("ff02::1%{iface_idx}")])
        .output();

    let Ok(output) = winproc::command("netsh")
        .args(["interface", "ipv6", "show", "neighbors", &format!("interface={iface_name}")])
        .output()
    else {
        return Vec::new();
    };

    parse_neighbor_table(&String::from_utf8_lossy(&output.stdout), iface_idx)
}

/// Parses `netsh interface ipv6 show neighbors` output into live link-local
/// neighbors only: drops multicast-group rows (`ff02::...`, always
/// `33-33-`-prefixed and state `Permanent`), and drops `Unreachable`/
/// `Incomplete` rows (stale cache entries or ones that never resolved).
/// A row can have no resolved MAC at all (`Incomplete`, Physical-Address
/// column literally reads "Unreachable") — handled by checking whether the
/// second token actually looks like a MAC rather than assuming its position.
pub fn parse_neighbor_table(text: &str, iface_idx: u32) -> Vec<Ipv6Neighbor> {
    let mut out = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("fe80:") {
            continue; // skips blank lines, headers, the "Interface N: name"
            // line, the column header row, the dashed separator, and every
            // non-link-local (ff02:: multicast, etc) row in one check.
        }

        let mut tokens = line.split_whitespace();
        let Some(address) = tokens.next() else { continue };
        let rest: Vec<&str> = tokens.collect();

        let (mac, state_tokens): (Option<&str>, &[&str]) = match rest.split_first() {
            Some((first, remainder)) if looks_like_mac(first) => (Some(*first), remainder),
            Some((_, _)) => (None, &rest[..]),
            None => continue,
        };
        let state = state_tokens.join(" ");

        if state.contains("Unreachable") || state.contains("Incomplete") {
            continue;
        }

        out.push(Ipv6Neighbor {
            address_with_zone: format!("{address}%{iface_idx}"),
            mac: mac.map(str::to_string),
        });
    }

    out
}

/// "aa-bb-cc-dd-ee-ff" (Windows' netsh MAC format) — six hex-pair groups.
pub fn looks_like_mac(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 6 && groups.iter().all(|g| g.len() == 2 && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Splits "fe80::1%20" -> ("fe80::1", Some("20")). `std::net`'s address
/// parsers don't understand the `%zone` suffix at all, so anywhere we need
/// a bare address for `Ipv6Addr::from_str`/`IpAddr::from_str`, split first.
pub fn split_zone(addr: &str) -> (&str, Option<&str>) {
    match addr.split_once('%') {
        Some((base, zone)) => (base, Some(zone)),
        None => (addr, None),
    }
}

/// RFC 6874 formatting for use in a browser URL: the zone delimiter `%`
/// itself must be percent-encoded as `%25` inside the bracketed literal.
/// "fe80::1%20" -> "[fe80::1%2520]".
pub fn ipv6_url_host(addr_with_zone: &str) -> String {
    match split_zone(addr_with_zone) {
        (base, Some(zone)) => format!("[{base}%25{zone}]"),
        (base, None) => format!("[{base}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output captured from `netsh interface ipv6 show neighbors
    // interface="WiFi"` on this machine: one genuine live neighbor (the
    // WiFi hotspot's own link-local address, already resolved via
    // background neighbor discovery) plus a run of multicast-group rows.
    const WIFI_FIXTURE: &str = "Interface 20: WiFi\r\n\
\r\n\
Internet Address                              Physical Address   Type\r\n\
--------------------------------------------  -----------------  -----------\r\n\
fe80::fc9c:a7ff:fec8:dc64                     fe-9c-a7-c8-dc-64  Reachable (Router)\r\n\
ff02::1                                       33-33-00-00-00-01  Permanent \r\n\
ff02::2                                       33-33-00-00-00-02  Permanent \r\n\
ff02::c                                       33-33-00-00-00-0c  Permanent \r\n\
ff02::16                                      33-33-00-00-00-16  Permanent \r\n\
ff02::fb                                      33-33-00-00-00-fb  Permanent \r\n\
ff02::1:2                                     33-33-00-01-00-02  Permanent \r\n\
ff02::1:3                                     33-33-00-01-00-03  Permanent \r\n\
ff02::1:ff69:2a6                              33-33-ff-69-02-a6  Permanent \r\n\
ff02::1:ffc8:dc64                             33-33-ff-c8-dc-64  Permanent \r\n";

    // Real output captured from the same command against "Ethernet 2" (a
    // disconnected/APIPA adapter): stale entries with an all-zero MAC and
    // state "Unreachable", plus one entry with NO resolved MAC at all
    // (Physical-Address column reads literally "Unreachable", state
    // "Incomplete") — the tricky column-shift case.
    const ETHERNET2_FIXTURE: &str = "Interface 12: Ethernet 2\r\n\
\r\n\
Internet Address                              Physical Address   Type\r\n\
--------------------------------------------  -----------------  -----------\r\n\
fe80::71b1:b8c8:dd69:2a6                      00-00-00-00-00-00  Unreachable \r\n\
fe80::a65d:36ff:fe62:5a85                     00-00-00-00-00-00  Unreachable \r\n\
fe80::fc9c:a7ff:fec8:dc64                     Unreachable        Incomplete \r\n\
ff02::1                                       33-33-00-00-00-01  Permanent \r\n\
ff02::2                                       33-33-00-00-00-02  Permanent \r\n";

    #[test]
    fn finds_the_one_real_neighbor_and_appends_zone() {
        let neighbors = parse_neighbor_table(WIFI_FIXTURE, 20);
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].address_with_zone, "fe80::fc9c:a7ff:fec8:dc64%20");
        assert_eq!(neighbors[0].mac.as_deref(), Some("fe-9c-a7-c8-dc-64"));
    }

    #[test]
    fn excludes_multicast_group_rows() {
        let neighbors = parse_neighbor_table(WIFI_FIXTURE, 20);
        assert!(!neighbors.iter().any(|n| n.address_with_zone.starts_with("ff02:")));
    }

    #[test]
    fn excludes_unreachable_and_incomplete_rows() {
        let neighbors = parse_neighbor_table(ETHERNET2_FIXTURE, 12);
        assert!(neighbors.is_empty(), "expected no live neighbors, got {neighbors:?}");
    }

    #[test]
    fn handles_row_with_no_resolved_mac_without_misparsing_state_as_mac() {
        // Even though this row's "Incomplete" state gets filtered out, the
        // parser must not crash or misread "Unreachable" (in the MAC
        // column position) as if it were a MAC address.
        let text = "fe80::fc9c:a7ff:fec8:dc64                     Unreachable        Incomplete \r\n";
        let neighbors = parse_neighbor_table(text, 12);
        assert!(neighbors.is_empty());
    }

    #[test]
    fn parse_neighbor_table_handles_empty_input() {
        assert!(parse_neighbor_table("", 1).is_empty());
    }

    #[test]
    fn looks_like_mac_accepts_valid_mac() {
        assert!(looks_like_mac("fe-9c-a7-c8-dc-64"));
        assert!(looks_like_mac("00-00-00-00-00-00"));
    }

    #[test]
    fn looks_like_mac_rejects_non_mac_tokens() {
        assert!(!looks_like_mac("Unreachable"));
        assert!(!looks_like_mac("Reachable"));
        assert!(!looks_like_mac(""));
        assert!(!looks_like_mac("fe-9c-a7-c8-dc")); // only 5 groups
    }

    #[test]
    fn split_zone_splits_address_and_zone() {
        assert_eq!(split_zone("fe80::1%20"), ("fe80::1", Some("20")));
    }

    #[test]
    fn split_zone_handles_address_without_zone() {
        assert_eq!(split_zone("fe80::1"), ("fe80::1", None));
    }

    #[test]
    fn ipv6_url_host_percent_encodes_zone_delimiter() {
        assert_eq!(ipv6_url_host("fe80::1%20"), "[fe80::1%2520]");
    }

    #[test]
    fn ipv6_url_host_handles_address_without_zone() {
        assert_eq!(ipv6_url_host("fe80::1"), "[fe80::1]");
    }
}
