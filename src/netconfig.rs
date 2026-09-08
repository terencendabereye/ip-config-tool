//! Applying and reverting static IPv4 configuration via `netsh`, with a
//! crash-safe on-disk backup of the interface's original configuration.

use crate::netiface::{self, InterfaceConfig, SCHEMA_VERSION};
use crate::winproc;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

const BACKUP_PREFIX: &str = "ip-config-tool-backup-";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupFile {
    pub schema_version: u32,
    pub config: InterfaceConfig,
}

/// Filesystem-safe encoding of an interface name for use in a backup filename.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

pub fn backup_path(iface_name: &str) -> PathBuf {
    env::temp_dir().join(format!("{BACKUP_PREFIX}{}.json", sanitize_name(iface_name)))
}

pub fn save_backup(config: &InterfaceConfig) -> Result<(), String> {
    let backup = BackupFile { schema_version: SCHEMA_VERSION, config: config.clone() };
    let json = serde_json::to_string_pretty(&backup).map_err(|e| e.to_string())?;
    fs::write(backup_path(&config.name), json).map_err(|e| format!("failed to write backup: {e}"))
}

pub fn load_backup(iface_name: &str) -> Option<BackupFile> {
    let path = backup_path(iface_name);
    let text = fs::read_to_string(path).ok()?;
    let backup: BackupFile = serde_json::from_str(&text).ok()?;
    if backup.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(backup)
}

pub fn delete_backup(iface_name: &str) {
    let _ = fs::remove_file(backup_path(iface_name));
}

pub fn has_backup(iface_name: &str) -> bool {
    backup_path(iface_name).exists()
}

/// Scans the temp directory for backups left behind by a previous run that
/// crashed or was force-closed before reverting.
pub fn list_orphan_backups() -> Vec<BackupFile> {
    let dir = env::temp_dir();
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return found };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !file_name.starts_with(BACKUP_PREFIX) || !file_name.ends_with(".json") {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(backup) = serde_json::from_str::<BackupFile>(&text) {
                if backup.schema_version == SCHEMA_VERSION {
                    found.push(backup);
                }
            }
        }
    }

    found
}

fn run_netsh(args: &[String]) -> Result<(), String> {
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = winproc::command("netsh")
        .args(&arg_refs)
        .output()
        .map_err(|e| format!("failed to launch netsh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!("netsh {arg_refs:?} failed: {stderr}{stdout}"));
    }
    Ok(())
}

/// Applies a static IPv4 address/mask/(optional) gateway to the interface.
/// Callers must validate inputs first (see `validate::validate_static_config`).
pub fn apply_static(iface: &str, ip: &str, mask: &str, gateway: &str) -> Result<(), String> {
    let mut args = vec![
        "interface".to_string(),
        "ip".to_string(),
        "set".to_string(),
        "address".to_string(),
        format!("name={iface}"),
        "static".to_string(),
        ip.to_string(),
        mask.to_string(),
    ];
    if !gateway.trim().is_empty() {
        args.push(gateway.trim().to_string());
    }
    run_netsh(&args)
}

fn set_dhcp(iface: &str) -> Result<(), String> {
    run_netsh(&[
        "interface".to_string(),
        "ip".to_string(),
        "set".to_string(),
        "address".to_string(),
        format!("name={iface}"),
        "dhcp".to_string(),
    ])?;
    run_netsh(&[
        "interface".to_string(),
        "ip".to_string(),
        "set".to_string(),
        "dns".to_string(),
        format!("name={iface}"),
        "dhcp".to_string(),
    ])
}

fn set_static_dns(iface: &str, dns: &[String]) -> Result<(), String> {
    for (i, server) in dns.iter().enumerate() {
        let action = if i == 0 { "set" } else { "add" };
        run_netsh(&[
            "interface".to_string(),
            "ip".to_string(),
            action.to_string(),
            "dns".to_string(),
            format!("name={iface}"),
            server.clone(),
        ])?;
    }
    Ok(())
}

/// Captures the interface's current configuration and persists it to disk
/// before any change is made. Must be called before the first `apply_static`.
pub fn backup_current_config(iface: &str) -> Result<InterfaceConfig, String> {
    let config = netiface::get_interface_config(iface)?;
    save_backup(&config)?;
    Ok(config)
}

/// Restores the interface to whatever was captured by `backup_current_config`.
/// Deletes the backup file on success so a stale backup can't be re-applied.
pub fn revert(iface: &str) -> Result<(), String> {
    let backup = load_backup(iface).ok_or_else(|| {
        format!("No saved backup found for '{iface}' — nothing to revert to")
    })?;

    let cfg = &backup.config;
    if cfg.dhcp {
        set_dhcp(iface)?;
    } else if let (Some(ip), Some(mask)) = (&cfg.ip, &cfg.mask) {
        apply_static(iface, ip, mask, cfg.gateway.as_deref().unwrap_or(""))?;
        if !cfg.dns.is_empty() {
            set_static_dns(iface, &cfg.dns)?;
        }
    } else {
        // Static-but-unconfigured (no IP at all) is unusual but not impossible
        // (e.g. a disabled adapter) — nothing further to restore.
    }

    delete_backup(iface);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn sample_config(name: &str) -> InterfaceConfig {
        InterfaceConfig {
            name: name.to_string(),
            dhcp: false,
            ip: Some("192.168.1.250".to_string()),
            mask: Some("255.255.255.0".to_string()),
            gateway: Some("192.168.1.1".to_string()),
            dns: vec!["8.8.8.8".to_string()],
        }
    }

    #[test]
    fn sanitize_name_strips_unsafe_chars() {
        assert_eq!(sanitize_name("Ethernet 2"), "Ethernet_2");
        assert_eq!(sanitize_name("vEthernet (Default Switch)"), "vEthernet__Default_Switch_");
    }

    #[test]
    #[serial]
    fn backup_round_trips_through_disk() {
        let cfg = sample_config("__unit_test_iface_a__");
        save_backup(&cfg).unwrap();

        let loaded = load_backup(&cfg.name).expect("backup should be found");
        assert_eq!(loaded.config, cfg);
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);

        delete_backup(&cfg.name);
        assert!(load_backup(&cfg.name).is_none());
    }

    #[test]
    #[serial]
    fn missing_backup_returns_none() {
        assert!(load_backup("__unit_test_iface_does_not_exist__").is_none());
    }

    #[test]
    #[serial]
    fn wrong_schema_version_is_ignored() {
        let cfg = sample_config("__unit_test_iface_b__");
        let mut backup = BackupFile { schema_version: SCHEMA_VERSION, config: cfg.clone() };
        backup.schema_version = SCHEMA_VERSION + 999;
        let json = serde_json::to_string_pretty(&backup).unwrap();
        std::fs::write(backup_path(&cfg.name), json).unwrap();

        assert!(load_backup(&cfg.name).is_none());

        delete_backup(&cfg.name);
    }

    #[test]
    #[serial]
    fn orphan_scan_finds_saved_backups() {
        let cfg = sample_config("__unit_test_iface_orphan__");
        save_backup(&cfg).unwrap();

        let orphans = list_orphan_backups();
        assert!(orphans.iter().any(|b| b.config.name == cfg.name));

        delete_backup(&cfg.name);
    }

    #[test]
    fn has_backup_reflects_disk_state() {
        let cfg = sample_config("__unit_test_iface_has__");
        assert!(!has_backup(&cfg.name));
        save_backup(&cfg).unwrap();
        assert!(has_backup(&cfg.name));
        delete_backup(&cfg.name);
        assert!(!has_backup(&cfg.name));
    }
}
