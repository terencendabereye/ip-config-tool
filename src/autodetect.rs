//! Cycling the adapter through common private/industrial subnets to find one
//! with a live device on it. The decision logic (`pick_match`) is pure and
//! unit-tested; the orchestration (`run`) drives real netsh/ping calls and is
//! exercised only by manual/integration testing (see the plan's test scheme).

use crate::netconfig;
use crate::netiface::InterfaceConfig;
use crate::scanner::{self, ScanResult};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// (network base e.g. "192.168.1", mask) in priority order.
pub const CANDIDATES: &[(&str, &str)] = &[
    ("192.168.1", "255.255.255.0"),
    ("192.168.0", "255.255.255.0"),
    ("192.168.10", "255.255.255.0"),
    ("10.0.0", "255.255.255.0"),
    ("10.0.1", "255.255.255.0"),
    ("172.16.0", "255.255.255.0"),
];

/// The probe address we temporarily assign ourselves while testing a candidate
/// subnet: high-numbered to minimise collision odds with real devices.
pub const PROBE_HOST_OCTET: u8 = 250;

#[derive(Debug, Clone, PartialEq)]
pub enum AutoDetectEvent {
    TryingCandidate { base: String, mask: String },
    CandidateResult { base: String, host_count: usize },
    Found { base: String, mask: String },
    NotFound,
    Cancelled,
    Error(String),
}

/// Pure decision function: given the ARP results gathered for one candidate
/// subnet, does it count as "a live device found"? We must exclude our own
/// probe address from counting as a match.
pub fn candidate_has_match(base: &str, probe_ip: &str, results: &[ScanResult]) -> bool {
    let _ = base;
    results.iter().any(|r| r.ip != probe_ip)
}

/// Given the ordered candidate list and a closure that scans one candidate,
/// returns the first candidate that has a match, or None if none did.
/// This is the pure "stop on first match / fall through on no match" state
/// machine that `run` below follows; kept separate (and unit-tested here with
/// a mocked scan-result provider) so the control-flow logic itself can be
/// verified without spawning real threads or touching a real adapter.
#[allow(dead_code)]
pub fn pick_match<F>(candidates: &[(&str, &str)], mut scan_candidate: F) -> Option<(String, String)>
where
    F: FnMut(&str, &str) -> Vec<ScanResult>,
{
    for (base, mask) in candidates {
        let probe_ip = format!("{base}.{PROBE_HOST_OCTET}");
        let results = scan_candidate(base, mask);
        if candidate_has_match(base, &probe_ip, &results) {
            return Some((base.to_string(), mask.to_string()));
        }
    }
    None
}

/// Runs the real auto-detect flow against `iface`, restoring the interface's
/// original config on completion (cancel, no match, or error) unless a match
/// is found, in which case the interface is left on the matching subnet for
/// the caller to confirm/apply for real. Always call with a fresh backup
/// already saved via `netconfig::backup_current_config`.
pub fn run(
    iface: &str,
    original: &InterfaceConfig,
    events: Sender<AutoDetectEvent>,
    cancelled: Arc<Mutex<bool>>,
) {
    for (base, mask) in CANDIDATES {
        if *cancelled.lock().unwrap() {
            let _ = events.send(AutoDetectEvent::Cancelled);
            restore_original(iface, original);
            return;
        }

        let _ = events.send(AutoDetectEvent::TryingCandidate { base: base.to_string(), mask: mask.to_string() });

        let probe_ip = format!("{base}.{PROBE_HOST_OCTET}");
        if let Err(e) = netconfig::apply_static(iface, &probe_ip, mask, "") {
            let _ = events.send(AutoDetectEvent::Error(format!("Failed to probe {base}.0/24: {e}")));
            restore_original(iface, original);
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        scanner::sweep_subnet_v24(base, tx, Arc::clone(&cancelled));
        let results: Vec<ScanResult> = rx.try_iter().collect();

        let _ = events.send(AutoDetectEvent::CandidateResult { base: base.to_string(), host_count: results.len() });

        if candidate_has_match(base, &probe_ip, &results) {
            let _ = events.send(AutoDetectEvent::Found { base: base.to_string(), mask: mask.to_string() });
            return;
        }
    }

    let _ = events.send(AutoDetectEvent::NotFound);
    restore_original(iface, original);
}

/// Restores the interface to its pre-probe state. `netconfig::revert` reads
/// the backup the caller saved before calling `run`, so once this returns the
/// interface is back to normal and there is nothing left to revert from.
fn restore_original(iface: &str, original: &InterfaceConfig) {
    let _ = original; // kept in the signature for clarity at call sites
    let _ = netconfig::revert(iface);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(ip: &str) -> ScanResult {
        ScanResult { ip: ip.to_string(), mac: "AA-BB-CC-DD-EE-FF".to_string() }
    }

    #[test]
    fn candidate_with_a_response_is_a_match() {
        let results = vec![hit("192.168.1.5")];
        assert!(candidate_has_match("192.168.1", "192.168.1.250", &results));
    }

    #[test]
    fn candidate_with_only_the_probes_own_ip_is_not_a_match() {
        let results = vec![hit("192.168.1.250")];
        assert!(!candidate_has_match("192.168.1", "192.168.1.250", &results));
    }

    #[test]
    fn candidate_with_no_responses_is_not_a_match() {
        assert!(!candidate_has_match("192.168.1", "192.168.1.250", &[]));
    }

    #[test]
    fn pick_match_stops_at_first_matching_candidate_in_priority_order() {
        let candidates: Vec<(&str, &str)> = vec![("192.168.1", "255.255.255.0"), ("10.0.0", "255.255.255.0")];
        let mut tried = Vec::new();

        let result = pick_match(&candidates, |base, _mask| {
            tried.push(base.to_string());
            if base == "192.168.1" {
                vec![hit("192.168.1.9")]
            } else {
                vec![hit("10.0.0.9")]
            }
        });

        assert_eq!(result, Some(("192.168.1".to_string(), "255.255.255.0".to_string())));
        // Must not have tried the second candidate once the first matched.
        assert_eq!(tried, vec!["192.168.1"]);
    }

    #[test]
    fn pick_match_falls_through_to_second_candidate_when_first_has_no_match() {
        let candidates: Vec<(&str, &str)> = vec![("192.168.1", "255.255.255.0"), ("10.0.0", "255.255.255.0")];

        let result = pick_match(&candidates, |base, _mask| {
            if base == "192.168.1" {
                vec![]
            } else {
                vec![hit("10.0.0.9")]
            }
        });

        assert_eq!(result, Some(("10.0.0".to_string(), "255.255.255.0".to_string())));
    }

    #[test]
    fn pick_match_returns_none_when_nothing_matches() {
        let candidates: Vec<(&str, &str)> = vec![("192.168.1", "255.255.255.0")];
        let result = pick_match(&candidates, |_base, _mask| vec![]);
        assert_eq!(result, None);
    }

    #[test]
    fn candidates_list_is_in_expected_priority_order() {
        assert_eq!(CANDIDATES[0], ("192.168.1", "255.255.255.0"));
        assert_eq!(CANDIDATES[1], ("192.168.0", "255.255.255.0"));
    }
}
