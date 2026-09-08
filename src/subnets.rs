//! User-saved custom subnets to try during auto-detect, on top of the
//! built-in common-range list (`autodetect::BUILTIN_CANDIDATES`). Persisted
//! to `%APPDATA%\ip-config-tool\custom_subnets.json` so a subnet read off a
//! device's own HMI/settings screen is remembered across runs, not just
//! for the current session — the next visit to the same site/vendor gear
//! doesn't need re-discovering.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomSubnet {
    /// First three octets, e.g. "198.120.0".
    pub base: String,
    pub mask: String,
    pub label: Option<String>,
}

fn storage_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("ip-config-tool").join("custom_subnets.json"))
}

pub fn load() -> Vec<CustomSubnet> {
    let Some(path) = storage_path() else { return Vec::new() };
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(list: &[CustomSubnet]) -> Result<(), String> {
    let path = storage_path().ok_or("no %APPDATA% directory available")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed to create config directory: {e}"))?;
    }
    let json = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| format!("failed to write custom_subnets.json: {e}"))
}

/// True if `base`/`mask` is already in `list` (exact match on both fields).
pub fn contains(list: &[CustomSubnet], base: &str, mask: &str) -> bool {
    list.iter().any(|c| c.base == base && c.mask == mask)
}

/// Removes the entry with this exact base/mask, if present. Returns whether
/// anything was actually removed.
pub fn remove(list: &mut Vec<CustomSubnet>, base: &str, mask: &str) -> bool {
    let before = list.len();
    list.retain(|c| !(c.base == base && c.mask == mask));
    list.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn contains_finds_exact_base_and_mask_match() {
        let list = vec![CustomSubnet { base: "198.120.0".into(), mask: "255.255.255.0".into(), label: None }];
        assert!(contains(&list, "198.120.0", "255.255.255.0"));
        assert!(!contains(&list, "198.120.0", "255.255.0.0"));
        assert!(!contains(&list, "10.0.0", "255.255.255.0"));
    }

    #[test]
    fn remove_deletes_matching_entry_and_reports_it() {
        let mut list = vec![
            CustomSubnet { base: "198.120.0".into(), mask: "255.255.255.0".into(), label: None },
            CustomSubnet { base: "10.10.10".into(), mask: "255.255.255.0".into(), label: None },
        ];
        assert!(remove(&mut list, "198.120.0", "255.255.255.0"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].base, "10.10.10");
    }

    #[test]
    fn remove_of_absent_entry_is_a_no_op_and_reports_false() {
        let mut list = vec![CustomSubnet { base: "10.10.10".into(), mask: "255.255.255.0".into(), label: None }];
        assert!(!remove(&mut list, "198.120.0", "255.255.255.0"));
        assert_eq!(list.len(), 1);
    }

    #[test]
    #[serial]
    fn save_and_load_round_trip_through_disk() {
        // This touches the same file a real user's saved subnets live in, so
        // capture whatever's already there first and restore it exactly
        // afterwards — never just derive a "cleaned" state from what the
        // test itself wrote, in case that logic is what's under test.
        let original = load();

        let mut with_test_entry = original.clone();
        with_test_entry.push(CustomSubnet {
            base: "__unit_test_198.120.0".into(),
            mask: "255.255.255.0".into(),
            label: Some("NR Electric test gateway".into()),
        });
        save(&with_test_entry).expect("save should succeed");

        let loaded = load();
        assert!(loaded.iter().any(|c| c.base == "__unit_test_198.120.0"));

        save(&original).expect("restoring the original saved-subnets file should succeed");
        assert_eq!(load(), original);
    }
}
