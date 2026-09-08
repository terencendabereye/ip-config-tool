//! MAC vendor (OUI) lookup, fully offline — plant/OT networks routinely have
//! no internet access, so this can't call an online API. The table is the
//! IEEE's public MA-L (24-bit OUI) registry, fetched once from
//! https://standards-oui.ieee.org/oui/oui.csv, trimmed to `prefix,vendor`
//! and sorted by prefix (see `data/trim_oui.py`), then embedded at compile
//! time via `include_str!` and binary-searched at lookup time.

use std::sync::OnceLock;

const RAW_TABLE: &str = include_str!("../data/oui.csv");

static TABLE: OnceLock<Vec<(u32, &'static str)>> = OnceLock::new();

fn table() -> &'static [(u32, &'static str)] {
    TABLE.get_or_init(|| parse_table(RAW_TABLE))
}

fn parse_table(raw: &'static str) -> Vec<(u32, &'static str)> {
    let mut rows: Vec<(u32, &'static str)> = raw
        .lines()
        .filter_map(|line| {
            let (prefix, vendor) = line.split_once(',')?;
            let prefix = u32::from_str_radix(prefix, 16).ok()?;
            Some((prefix, vendor))
        })
        .collect();
    rows.sort_unstable_by_key(|(prefix, _)| *prefix);
    rows
}

/// Looks up the vendor for a MAC's first 3 bytes (its OUI). Returns `None`
/// if the prefix isn't in IEEE's registry (locally-administered/randomized
/// MACs, or a prefix newer than the embedded snapshot).
pub fn vendor_lookup(oui: [u8; 3]) -> Option<&'static str> {
    let key = u32::from_be_bytes([0, oui[0], oui[1], oui[2]]);
    table().binary_search_by_key(&key, |(prefix, _)| *prefix).ok().map(|i| table()[i].1)
}

/// Convenience overload taking a full 6-byte MAC.
pub fn vendor_lookup_mac(mac: [u8; 6]) -> Option<&'static str> {
    vendor_lookup([mac[0], mac[1], mac[2]])
}

#[cfg(test)]
mod tests {
    use super::*;

    // A handful of real, stable IEEE-assigned OUIs to catch parsing/lookup
    // regressions without depending on the full 40k-row table's exact content.
    #[test]
    fn resolves_known_cisco_prefix() {
        assert_eq!(vendor_lookup([0x00, 0x00, 0x0C]).unwrap(), "Cisco Systems  Inc");
    }

    #[test]
    fn resolves_known_siemens_prefix() {
        let vendor = vendor_lookup([0x00, 0x01, 0xE3]).unwrap();
        assert!(vendor.contains("Siemens"), "unexpected vendor: {vendor}");
    }

    #[test]
    fn unknown_prefix_returns_none() {
        // FF-FF-FF is reserved/unassigned, never a real OUI.
        assert_eq!(vendor_lookup([0xFF, 0xFF, 0xFF]), None);
    }

    #[test]
    fn full_mac_overload_uses_only_first_three_bytes() {
        let mac = [0x00, 0x00, 0x0C, 0x12, 0x34, 0x56];
        assert_eq!(vendor_lookup_mac(mac), vendor_lookup([0x00, 0x00, 0x0C]));
    }

    #[test]
    fn parse_table_handles_small_fixture() {
        let fixture = "00000C,Cisco Systems  Inc\n0001E3,Siemens AG\n";
        let leaked: &'static str = Box::leak(fixture.to_string().into_boxed_str());
        let rows = parse_table(leaked);
        assert_eq!(rows.len(), 2);
        assert!(rows.windows(2).all(|w| w[0].0 <= w[1].0), "table must be sorted for binary search");
    }

    #[test]
    fn table_is_sorted_and_nonempty() {
        let t = table();
        assert!(t.len() > 10_000, "expected the full embedded IEEE table, got {} rows", t.len());
        assert!(t.windows(2).all(|w| w[0].0 <= w[1].0));
    }
}
