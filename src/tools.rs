//! Launching standard OS network tools against a discovered device, and
//! Wake-on-LAN. Ping/Traceroute/SSH intentionally open a normal *visible*
//! console window (unlike the hidden `netsh`/`arp` calls in `winproc`) since
//! they're meant to be watched/interacted with — see the plan for why an
//! in-app terminal (ConPTY) was deliberately deferred to a follow-up issue.

use crate::winproc;
use std::net::UdpSocket;
use std::process::Command;

/// Opens a new, visible console window running `program args...` via `cmd
/// /C start`, so the user can watch continuous output (`ping -t`) or
/// interact with it (an SSH session) and Ctrl+C when done.
pub fn launch_visible(program: &str, args: &[&str]) -> Result<(), String> {
    let mut cmd_args = vec!["/C", "start", program];
    cmd_args.extend_from_slice(args);
    Command::new("cmd")
        .args(&cmd_args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to launch {program}: {e}"))
}

/// Opens `url` in the user's default browser without flashing a console.
pub fn open_browser(url: &str) -> Result<(), String> {
    winproc::command("cmd")
        .args(["/C", "start", url])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to open browser: {e}"))
}

/// Builds the standard Wake-on-LAN "magic packet": 6 bytes of 0xFF followed
/// by the target MAC address repeated 16 times (102 bytes total).
pub fn build_magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0u8; 102];
    packet[..6].copy_from_slice(&[0xFF; 6]);
    for i in 0..16 {
        let start = 6 + i * 6;
        packet[start..start + 6].copy_from_slice(&mac);
    }
    packet
}

/// Parses a `"AA-BB-CC-DD-EE-FF"` (or `:`-separated) MAC string, as produced
/// by `scanner::format_mac`, back into raw bytes for Wake-on-LAN / identify.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let parts: Vec<&str> = text.split(['-', ':']).collect();
    if parts.len() != 6 {
        return None;
    }
    for (i, part) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(out)
}

/// Sends a Wake-on-LAN magic packet as a UDP broadcast on port 9.
pub fn wake_on_lan(mac: [u8; 6]) -> Result<(), String> {
    let packet = build_magic_packet(mac);
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("failed to open socket: {e}"))?;
    socket.set_broadcast(true).map_err(|e| format!("failed to enable broadcast: {e}"))?;
    socket
        .send_to(&packet, "255.255.255.255:9")
        .map_err(|e| format!("failed to send magic packet: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_is_102_bytes() {
        let packet = build_magic_packet([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(packet.len(), 102);
    }

    #[test]
    fn magic_packet_starts_with_six_ff_bytes() {
        let packet = build_magic_packet([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(&packet[..6], &[0xFF; 6]);
    }

    #[test]
    fn magic_packet_repeats_mac_sixteen_times() {
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        let packet = build_magic_packet(mac);
        for i in 0..16 {
            let start = 6 + i * 6;
            assert_eq!(&packet[start..start + 6], &mac, "repetition {i} mismatch");
        }
    }

    #[test]
    fn parse_mac_accepts_dash_separated() {
        assert_eq!(parse_mac("AA-BB-CC-DD-EE-FF"), Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
    }

    #[test]
    fn parse_mac_accepts_colon_separated() {
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff"), Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
    }

    #[test]
    fn parse_mac_rejects_wrong_segment_count() {
        assert_eq!(parse_mac("AA-BB-CC"), None);
    }

    #[test]
    fn parse_mac_rejects_non_hex() {
        assert_eq!(parse_mac("ZZ-BB-CC-DD-EE-FF"), None);
    }
}
