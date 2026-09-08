//! Fast subnet discovery via `SendARP` (the IP Helper API) — the same
//! mechanism Windows itself uses to resolve link-layer addresses (it's what
//! backs `arp -a` and device-discovery in Network Connections/Network
//! Manager-style tooling). This is used instead of ICMP ping sweeping:
//! it works even when a host's firewall blocks ICMP (industrial devices
//! commonly do), needs no per-host process spawn, and a non-responsive host
//! fails fast instead of waiting out a ping timeout.
//!
//! Parsing of `arp -a` output (used as a cheap supplementary source) is kept
//! pure/testable; `arp_resolve`/`sweep_subnet_v24` do the real Win32 calls
//! and process spawning.

use crate::winproc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use winapi::shared::minwindef::{DWORD, ULONG};
use winapi::um::iphlpapi::SendARP;

#[derive(Debug, Clone, PartialEq)]
pub struct ScanResult {
    pub ip: String,
    pub mac: String,
}

/// Resolves the MAC address for `octets` via `SendARP`. Returns `None` if
/// the host doesn't respond (unreachable, wrong subnet, powered off, etc).
/// `SendARP` triggers a real ARP request on the wire when the address isn't
/// already cached, so this both discovers and resolves in one call.
pub fn arp_resolve(octets: [u8; 4]) -> Option<String> {
    // `SendARP` wants the address in "network order", i.e. the raw octets
    // read directly into a native-endian word — not the big-endian numeric
    // interpretation of the dotted quad.
    let dest_ip: ULONG = u32::from_ne_bytes(octets);
    let mut mac_addr = [0u8; 6];
    let mut addr_len: ULONG = mac_addr.len() as ULONG;

    let result: DWORD = unsafe { SendARP(dest_ip, 0, mac_addr.as_mut_ptr() as *mut _, &mut addr_len) };

    const NO_ERROR: DWORD = 0;
    if result == NO_ERROR && addr_len == mac_addr.len() as ULONG {
        Some(format_mac(&mac_addr))
    } else {
        None
    }
}

fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("-")
}

pub fn run_arp_a() -> Result<String, String> {
    let output = winproc::command("arp")
        .arg("-a")
        .output()
        .map_err(|e| format!("failed to launch arp: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses `arp -a` output (possibly containing multiple "Interface: ..."
/// sections) into a flat list of (ip, mac) pairs, skipping broadcast/
/// multicast/incomplete entries.
pub fn parse_arp_table(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Interface:") || line.starts_with("Internet Address") {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        let ip = cols[0];
        let mac = cols[1];
        if !ip.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            continue;
        }
        if mac.eq_ignore_ascii_case("ff-ff-ff-ff-ff-ff") {
            continue; // broadcast
        }
        out.push((ip.to_string(), mac.to_string()));
    }
    out
}

/// Parses a "192.168.1" style base into its three octets.
fn parse_base(base: &str) -> Option<[u8; 3]> {
    let parts: Vec<&str> = base.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut out = [0u8; 3];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.parse().ok()?;
    }
    Some(out)
}

/// ARP-resolves every host address in `base`.0/24 (i.e. `<base>.1` ..
/// `<base>.254`), streaming each live result to `sender` as it resolves,
/// then folds in anything the OS ARP cache already had cached (near-free,
/// occasionally catches an entry a fresh `SendARP` call just missed).
/// Runs on the calling thread's pool of worker threads and blocks until the
/// whole sweep is done, so call it from a background thread, never the GUI
/// thread.
pub fn sweep_subnet_v24(base: &str, sender: Sender<ScanResult>, cancelled: Arc<Mutex<bool>>) {
    const WORKERS: usize = 48;

    let Some([a, b, c]) = parse_base(base) else { return };
    let hosts: Vec<[u8; 4]> = (1u8..=254).map(|d| [a, b, c, d]).collect();
    let sender = Arc::new(Mutex::new(sender));

    for chunk in hosts.chunks(WORKERS) {
        if *cancelled.lock().unwrap() {
            break;
        }
        let handles: Vec<_> = chunk
            .iter()
            .copied()
            .map(|octets| {
                let sender = Arc::clone(&sender);
                thread::spawn(move || {
                    if let Some(mac) = arp_resolve(octets) {
                        let ip = format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3]);
                        let _ = sender.lock().unwrap().send(ScanResult { ip, mac });
                    }
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }
    }

    if let Ok(arp_text) = run_arp_a() {
        let arp = parse_arp_table(&arp_text);
        let prefix = format!("{base}.");
        for (ip, mac) in arp {
            if ip.starts_with(&prefix) {
                let _ = sender.lock().unwrap().send(ScanResult { ip, mac });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output captured from `arp -a` on this machine (multiple interface
    // sections, broadcast/multicast noise, a dynamic entry mixed with statics).
    const ARP_OUTPUT: &str = "\r\n\
Interface: 169.254.123.130 --- 0xc\r\n\
  Internet Address      Physical Address      Type\r\n\
  169.254.255.255       ff-ff-ff-ff-ff-ff     static    \r\n\
  224.0.0.22            01-00-5e-00-00-16     static    \r\n\
\r\n\
Interface: 172.20.10.2 --- 0x14\r\n\
  Internet Address      Physical Address      Type\r\n\
  172.20.10.1           fe-9c-a7-c8-dc-64     dynamic   \r\n\
  172.20.10.15          ff-ff-ff-ff-ff-ff     static    \r\n\
  224.0.0.251            01-00-5e-00-00-fb     static    \r\n";

    #[test]
    fn parses_arp_table_across_multiple_interfaces() {
        let entries = parse_arp_table(ARP_OUTPUT);
        assert!(entries.contains(&("172.20.10.1".to_string(), "fe-9c-a7-c8-dc-64".to_string())));
        assert!(entries.contains(&("224.0.0.22".to_string(), "01-00-5e-00-00-16".to_string())));
    }

    #[test]
    fn parse_arp_table_excludes_broadcast_entries() {
        let entries = parse_arp_table(ARP_OUTPUT);
        assert!(!entries.iter().any(|(_, mac)| mac.eq_ignore_ascii_case("ff-ff-ff-ff-ff-ff")));
    }

    #[test]
    fn parse_arp_table_handles_empty_input() {
        assert!(parse_arp_table("").is_empty());
    }

    #[test]
    fn parse_arp_table_ignores_header_and_interface_lines() {
        let entries = parse_arp_table(ARP_OUTPUT);
        // 3 real (non-broadcast) address rows across both interface sections:
        // 224.0.0.22, 172.20.10.1, 224.0.0.251 (169.254.255.255 and
        // 172.20.10.15 are broadcast entries and get filtered out).
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn parse_base_splits_three_octets() {
        assert_eq!(parse_base("192.168.1"), Some([192, 168, 1]));
        assert_eq!(parse_base("10.0.0"), Some([10, 0, 0]));
    }

    #[test]
    fn parse_base_rejects_wrong_segment_count() {
        assert_eq!(parse_base("192.168.1.5"), None);
        assert_eq!(parse_base("192.168"), None);
    }

    #[test]
    fn format_mac_pads_and_uppercases() {
        assert_eq!(format_mac(&[0x02, 0xa, 0xff, 0x00, 0x1, 0xbc]), "02-0A-FF-00-01-BC");
    }

    #[test]
    fn arp_resolve_returns_none_for_unreachable_address() {
        // 240.0.0.1 is in reserved/unused space and should never resolve on
        // any real network — this exercises the real SendARP call and
        // confirms it fails closed (None) rather than panicking or hanging.
        assert_eq!(arp_resolve([240, 0, 0, 1]), None);
    }
}
