//! On-demand device identification: MAC vendor, hostname (reverse DNS,
//! falling back to NetBIOS — the latter is often the *only* way to get a
//! name on an isolated OT/plant LAN with no DNS server), and a quick scan of
//! a curated list of identification-relevant ports. Triggered per-device
//! when the user selects a row in the scan results, never automatically for
//! every host (that would slow the bulk ARP sweep down for no benefit).

use crate::ipv6;
use crate::oui;
use crate::winproc;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6, TcpStream};
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Ports worth checking to help identify what kind of device this is, and to
/// back the "Open web UI" action. Industrial protocol ports included
/// alongside common IT ones since this tool's primary audience is PLCs/RTUs/
/// meters as well as ordinary network gear.
pub const PORTS_TO_CHECK: &[(u16, &str)] = &[
    (21, "FTP"),
    (22, "SSH"),
    (23, "Telnet"),
    (80, "HTTP"),
    (102, "S7comm"),
    (443, "HTTPS"),
    (445, "SMB"),
    (502, "Modbus"),
    (3389, "RDP"),
    (20000, "DNP3"),
    (44818, "EtherNet/IP"),
];

const PORT_TIMEOUT: Duration = Duration::from_millis(350);
const DNS_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IdentifyResult {
    pub vendor: Option<String>,
    pub hostname: Option<String>,
    pub open_ports: Vec<&'static str>,
}

/// Runs the full identification for one device. Blocking (DNS + up to 11
/// port probes with short timeouts) — always call from a background thread,
/// never the GUI thread.
///
/// `mac` is `None` for an IPv6 neighbor-table entry that never resolved a
/// link-layer address (state "Incomplete" — see `ipv6::parse_neighbor_table`);
/// vendor lookup is skipped in that case rather than guessing.
pub fn identify(ip: &str, mac: Option<[u8; 6]>) -> IdentifyResult {
    let vendor = mac.and_then(oui::vendor_lookup_mac).map(|v| v.to_string());

    // No PTR records for link-local addresses, and NetBIOS is IPv4-only —
    // neither applies to an IPv6 target, so don't waste the round trips.
    let hostname = if is_ipv6(ip) { None } else { reverse_dns_with_timeout(ip).or_else(|| netbios_name(ip)) };

    let open_ports = port_scan(ip, PORTS_TO_CHECK);

    IdentifyResult { vendor, hostname, open_ports }
}

fn is_ipv6(ip: &str) -> bool {
    ip.contains(':')
}

/// `dns_lookup::lookup_addr` has no built-in timeout and an unresponsive/
/// misconfigured DNS server on an OT network could hang for a long time, so
/// this runs it on a helper thread and gives up after `DNS_TIMEOUT`.
fn reverse_dns_with_timeout(ip: &str) -> Option<String> {
    let addr = IpAddr::from_str(ip).ok()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(dns_lookup::lookup_addr(&addr).ok());
    });
    rx.recv_timeout(DNS_TIMEOUT).ok().flatten().filter(|name| !name.is_empty() && name != ip)
}

pub fn netbios_name(ip: &str) -> Option<String> {
    let output = winproc::command("nbtstat").args(["-A", ip]).output().ok()?;
    parse_netbios_name(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `nbtstat -A <ip>` output. Prefers the `<20>` (workstation service)
/// entry since it reliably identifies an actual machine name rather than a
/// workgroup/domain (`<00> GROUP`); falls back to `<00> UNIQUE`.
pub fn parse_netbios_name(text: &str) -> Option<String> {
    if text.contains("Host not found") {
        return None;
    }

    let mut fallback: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        let Some(name) = line.split_whitespace().next() else { continue };
        if line.contains("<20>") && line.contains("UNIQUE") {
            return Some(name.to_string());
        }
        if fallback.is_none() && line.contains("<00>") && line.contains("UNIQUE") {
            fallback = Some(name.to_string());
        }
    }
    fallback
}

/// Base address to probe, resolved once up front rather than per-port.
/// `Ipv6Addr::from_str`/`IpAddr::from_str` don't understand a `%zone`
/// suffix at all, so a scoped link-local literal (e.g.
/// "fe80::1%20") needs its zone split off and fed to `SocketAddrV6`
/// directly instead of through string parsing.
#[derive(Clone, Copy)]
enum Target {
    V4(IpAddr),
    V6 { addr: Ipv6Addr, scope_id: u32 },
}

fn resolve_target(ip: &str) -> Option<Target> {
    let (base, zone) = ipv6::split_zone(ip);
    match zone {
        Some(zone) => Some(Target::V6 { addr: Ipv6Addr::from_str(base).ok()?, scope_id: zone.parse().ok()? }),
        None => Some(Target::V4(IpAddr::from_str(base).ok()?)),
    }
}

/// Probes each port in `ports` in parallel with a short connect timeout,
/// returning the labels of the ones that accepted a connection.
pub fn port_scan(ip: &str, ports: &[(u16, &'static str)]) -> Vec<&'static str> {
    let Some(target) = resolve_target(ip) else { return Vec::new() };

    let handles: Vec<_> = ports
        .iter()
        .map(|&(port, label)| {
            let sock = match target {
                Target::V4(addr) => SocketAddr::new(addr, port),
                Target::V6 { addr, scope_id } => SocketAddr::V6(SocketAddrV6::new(addr, port, 0, scope_id)),
            };
            thread::spawn(move || TcpStream::connect_timeout(&sock, PORT_TIMEOUT).is_ok().then_some(label))
        })
        .collect();

    handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output captured on this machine: every adapter section ends in
    // "Host not found." (no live NetBIOS-capable host was reachable here).
    const NO_RESULT: &str = "\r\n\
WiFi:\r\n\
Node IpAddress: [172.20.10.2] Scope Id: []\r\n\
\r\n\
    Host not found.\r\n";

    #[test]
    fn parse_netbios_name_returns_none_when_host_not_found() {
        assert_eq!(parse_netbios_name(NO_RESULT), None);
    }

    // Microsoft's standard, long-stable `nbtstat -A` success format (see
    // Microsoft Learn's nbtstat docs) — no live NetBIOS host was reachable on
    // this dev machine's network to capture a real positive example, but this
    // layout has been unchanged across Windows versions for decades.
    const FOUND_RESULT: &str = "\r\n\
Local Area Connection:\r\n\
Node IpAddress: [192.168.1.5] Scope Id: []\r\n\
\r\n\
    NetBIOS Remote Machine Name Table\r\n\
\r\n\
    Name               Type         Status\r\n\
    ---------------------------------------------\r\n\
    ENGWORKSTATION <00>  UNIQUE      Registered\r\n\
    WORKGROUP      <00>  GROUP       Registered\r\n\
    ENGWORKSTATION <20>  UNIQUE      Registered\r\n\
    WORKGROUP      <1E>  GROUP       Registered\r\n\
\r\n\
    MAC Address = 00-11-22-33-44-55\r\n";

    #[test]
    fn parse_netbios_name_prefers_the_20_workstation_entry() {
        assert_eq!(parse_netbios_name(FOUND_RESULT).as_deref(), Some("ENGWORKSTATION"));
    }

    #[test]
    fn parse_netbios_name_ignores_group_entries() {
        let name = parse_netbios_name(FOUND_RESULT).unwrap();
        assert_ne!(name, "WORKGROUP");
    }

    #[test]
    fn parse_netbios_name_falls_back_to_00_unique_without_20() {
        let text = "\r\nName Table\r\n    SOMEHOST       <00>  UNIQUE      Registered\r\n    WORKGROUP      <00>  GROUP       Registered\r\n";
        assert_eq!(parse_netbios_name(text).as_deref(), Some("SOMEHOST"));
    }

    #[test]
    fn parse_netbios_name_handles_empty_input() {
        assert_eq!(parse_netbios_name(""), None);
    }

    #[test]
    fn port_scan_of_localhost_finds_no_open_ports_in_reserved_range() {
        // 240.0.0.1 is unused/reserved space; nothing should be listening.
        let result = port_scan("240.0.0.1", &[(1, "test")]);
        assert!(result.is_empty());
    }

    #[test]
    fn port_scan_handles_invalid_ip_gracefully() {
        assert!(port_scan("not-an-ip", PORTS_TO_CHECK).is_empty());
    }

    #[test]
    fn port_scan_accepts_scoped_ipv6_link_local_address() {
        // Reserved/unused-in-practice link-local address; nothing should be
        // listening, but this exercises the %zone parsing path end to end
        // without hanging or erroring out.
        let result = port_scan("fe80::dead:beef%1", &[(1, "test")]);
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_target_splits_v6_zone_into_scope_id() {
        let target = resolve_target("fe80::1%20").expect("should parse");
        match target {
            Target::V6 { addr, scope_id } => {
                assert_eq!(addr, Ipv6Addr::from_str("fe80::1").unwrap());
                assert_eq!(scope_id, 20);
            }
            Target::V4(_) => panic!("expected V6 target"),
        }
    }

    #[test]
    fn resolve_target_rejects_non_numeric_zone() {
        assert!(resolve_target("fe80::1%not-a-number").is_none());
    }

    #[test]
    fn resolve_target_handles_plain_ipv4() {
        let target = resolve_target("192.168.1.1").expect("should parse");
        match target {
            Target::V4(addr) => assert_eq!(addr, IpAddr::from_str("192.168.1.1").unwrap()),
            Target::V6 { .. } => panic!("expected V4 target"),
        }
    }

    #[test]
    fn is_ipv6_distinguishes_address_families() {
        assert!(is_ipv6("fe80::1%20"));
        assert!(!is_ipv6("192.168.1.1"));
    }
}
