//! Enumerating network interfaces and reading their current IPv4 configuration
//! by shelling out to `netsh`. Parsing is kept separate from process-spawning
//! so it can be unit tested against captured real `netsh` output.

use crate::winproc;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceConfig {
    pub name: String,
    pub dhcp: bool,
    pub ip: Option<String>,
    pub mask: Option<String>,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterfaceSummary {
    pub idx: u32,
    pub name: String,
    pub state: String,
}

/// Runs `netsh interface ipv4 show interfaces` and returns the parsed rows.
pub fn list_interfaces() -> Result<Vec<InterfaceSummary>, String> {
    let output = run_netsh(&["interface", "ipv4", "show", "interfaces"])?;
    Ok(parse_interfaces_table(&output))
}

/// Runs `netsh interface ip show config name="<name>"` and returns the parsed config.
pub fn get_interface_config(name: &str) -> Result<InterfaceConfig, String> {
    let output = run_netsh(&["interface", "ip", "show", "config", &format!("name={name}")])?;
    parse_show_config(&output, name)
}

fn run_netsh(args: &[&str]) -> Result<String, String> {
    let output = winproc::command("netsh")
        .args(args)
        .output()
        .map_err(|e| format!("failed to launch netsh: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!("netsh {args:?} failed: {stderr}{stdout}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses the fixed-column `netsh interface ipv4 show interfaces` table.
/// Columns are `Idx  Met  MTU  State  Name`; only Name may contain spaces.
pub fn parse_interfaces_table(text: &str) -> Vec<InterfaceSummary> {
    let mut rows = Vec::new();
    let mut past_header = false;

    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if !past_header {
            // The separator row is a line of dashes/spaces only.
            if line.chars().all(|c| c == '-' || c.is_whitespace()) {
                past_header = true;
            }
            continue;
        }

        let trimmed = line.trim_start();
        let idx = match trimmed.split_whitespace().next().and_then(|v| v.parse::<u32>().ok()) {
            Some(v) => v,
            None => continue,
        };

        // Idx, Met, MTU, State are single whitespace-separated tokens; Name is
        // everything after them and may itself contain spaces.
        let mut tok = trimmed.split_whitespace();
        let _idx = tok.next();
        let _met = tok.next();
        let _mtu = tok.next();
        let state = tok.next().unwrap_or("").to_string();
        let name_start = find_nth_token_end(trimmed, 4);
        let name = trimmed[name_start..].trim().to_string();

        if state.is_empty() || name.is_empty() {
            continue;
        }

        rows.push(InterfaceSummary { idx, name, state });
    }

    rows
}

/// Returns the byte offset in `s` right after the end of the `n`th
/// whitespace-separated token (1-indexed), skipping leading whitespace first.
fn find_nth_token_end(s: &str, n: usize) -> usize {
    let mut count = 0;
    let mut in_token = false;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            if in_token {
                count += 1;
                in_token = false;
                if count == n {
                    return i;
                }
            }
        } else {
            in_token = true;
        }
    }
    if in_token {
        count += 1;
        if count == n {
            return s.len();
        }
    }
    s.len()
}

/// Parses `netsh interface ip show config name="..."` output.
pub fn parse_show_config(text: &str, fallback_name: &str) -> Result<InterfaceConfig, String> {
    let mut name = fallback_name.to_string();
    let mut dhcp = true;
    let mut ip: Option<String> = None;
    let mut mask: Option<String> = None;
    let mut gateway: Option<String> = None;
    let mut dns: Vec<String> = Vec::new();

    let mut in_dns_section = false;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() {
            continue;
        }

        if let Some(rest) = line.trim_start().strip_prefix("Configuration for interface") {
            name = rest.trim().trim_matches('"').to_string();
            in_dns_section = false;
            continue;
        }

        // Continuation lines (extra DNS servers) have no "Key:" prefix: they are
        // pure leading whitespace followed directly by a value.
        let trimmed = line.trim_start();
        let looks_like_key_value = trimmed
            .split(':')
            .next()
            .map(|k| !k.trim().is_empty() && k.len() < trimmed.len())
            .unwrap_or(false)
            && trimmed.contains(':');

        if !looks_like_key_value {
            if in_dns_section && !trimmed.is_empty() && trimmed != "None" {
                dns.push(trimmed.to_string());
            }
            continue;
        }

        let mut split = line.splitn(2, ':');
        let key = split.next().unwrap_or("").trim();
        let value = split.next().unwrap_or("").trim();

        match key {
            "DHCP enabled" => {
                dhcp = value.eq_ignore_ascii_case("yes");
                in_dns_section = false;
            }
            "IP Address" => {
                ip = Some(value.to_string());
                in_dns_section = false;
            }
            "Subnet Prefix" => {
                mask = extract_mask(value);
                in_dns_section = false;
            }
            "Default Gateway" => {
                if !value.is_empty() {
                    gateway = Some(value.to_string());
                }
                in_dns_section = false;
            }
            "DNS servers configured through DHCP" | "Statically Configured DNS Servers" => {
                in_dns_section = true;
                if !value.is_empty() && !value.eq_ignore_ascii_case("none") {
                    dns.push(value.to_string());
                }
            }
            _ => {
                in_dns_section = false;
            }
        }
    }

    Ok(InterfaceConfig { name, dhcp, ip, mask, gateway, dns })
}

/// "172.20.10.0/28 (mask 255.255.255.240)" -> Some("255.255.255.240")
fn extract_mask(subnet_prefix: &str) -> Option<String> {
    let start = subnet_prefix.find("mask ")? + "mask ".len();
    let rest = &subnet_prefix[start..];
    let end = rest.find(')').unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERFACES_TABLE: &str = "Idx     Met         MTU          State                Name\r\n\
---  ----------  ----------  ------------  ---------------------------\r\n\
  1          75  4294967295  connected     Loopback Pseudo-Interface 1\r\n\
 20          45        1500  connected     WiFi\r\n\
  5           5        1500  disconnected  Ethernet\r\n\
 12          25        1500  connected     Ethernet 2\r\n\
 45        5000        1500  connected     vEthernet (Default Switch)\r\n";

    #[test]
    fn parses_interfaces_table_names_with_spaces() {
        let rows = parse_interfaces_table(INTERFACES_TABLE);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Loopback Pseudo-Interface 1",
                "WiFi",
                "Ethernet",
                "Ethernet 2",
                "vEthernet (Default Switch)",
            ]
        );
    }

    #[test]
    fn parses_interfaces_table_state_and_idx() {
        let rows = parse_interfaces_table(INTERFACES_TABLE);
        let ethernet2 = rows.iter().find(|r| r.name == "Ethernet 2").unwrap();
        assert_eq!(ethernet2.idx, 12);
        assert_eq!(ethernet2.state, "connected");

        let ethernet = rows.iter().find(|r| r.name == "Ethernet").unwrap();
        assert_eq!(ethernet.state, "disconnected");
    }

    // Real output captured from `netsh interface ip show config name="WiFi"`.
    const DHCP_WITH_GATEWAY_AND_MULTILINE_DNS: &str = "\r\nConfiguration for interface \"WiFi\"\r\n\
    DHCP enabled:                         Yes\r\n\
    IP Address:                           172.20.10.2\r\n\
    Subnet Prefix:                        172.20.10.0/28 (mask 255.255.255.240)\r\n\
    Default Gateway:                      172.20.10.1\r\n\
    Gateway Metric:                       0\r\n\
    InterfaceMetric:                      45\r\n\
    DNS servers configured through DHCP:  127.0.0.1\r\n\
                                          1.1.1.1\r\n\
    Register with which suffix:           Primary only\r\n\
    WINS servers configured through DHCP: None\r\n";

    #[test]
    fn parses_dhcp_interface_with_gateway_and_multiline_dns() {
        let cfg = parse_show_config(DHCP_WITH_GATEWAY_AND_MULTILINE_DNS, "WiFi").unwrap();
        assert_eq!(cfg.name, "WiFi");
        assert!(cfg.dhcp);
        assert_eq!(cfg.ip.as_deref(), Some("172.20.10.2"));
        assert_eq!(cfg.mask.as_deref(), Some("255.255.255.240"));
        assert_eq!(cfg.gateway.as_deref(), Some("172.20.10.1"));
        assert_eq!(cfg.dns, vec!["127.0.0.1", "1.1.1.1"]);
    }

    // Real output captured from `netsh interface ip show config name="Ethernet 2"`
    // (link-local/APIPA address, DHCP disabled, no gateway).
    const STATIC_NO_GATEWAY_NO_DNS: &str = "\r\nConfiguration for interface \"Ethernet 2\"\r\n\
    DHCP enabled:                         No\r\n\
    IP Address:                           169.254.123.130\r\n\
    Subnet Prefix:                        169.254.0.0/16 (mask 255.255.0.0)\r\n\
    InterfaceMetric:                      25\r\n\
    Statically Configured DNS Servers:    None\r\n\
    Register with which suffix:           Primary only\r\n\
    Statically Configured WINS Servers:   None\r\n";

    #[test]
    fn parses_static_interface_without_gateway_or_dns() {
        let cfg = parse_show_config(STATIC_NO_GATEWAY_NO_DNS, "Ethernet 2").unwrap();
        assert_eq!(cfg.name, "Ethernet 2");
        assert!(!cfg.dhcp);
        assert_eq!(cfg.ip.as_deref(), Some("169.254.123.130"));
        assert_eq!(cfg.mask.as_deref(), Some("255.255.0.0"));
        assert_eq!(cfg.gateway, None);
        assert!(cfg.dns.is_empty());
    }

    // Real output captured from a disconnected DHCP adapter with no IP at all.
    const DHCP_DISCONNECTED_NO_IP: &str = "\r\nConfiguration for interface \"Ethernet\"\r\n\
    DHCP enabled:                         Yes\r\n\
    InterfaceMetric:                      5\r\n\
    DNS servers configured through DHCP:  None\r\n\
    Register with which suffix:           Primary only\r\n\
    WINS servers configured through DHCP: None\r\n";

    #[test]
    fn parses_disconnected_dhcp_interface_with_no_ip() {
        let cfg = parse_show_config(DHCP_DISCONNECTED_NO_IP, "Ethernet").unwrap();
        assert!(cfg.dhcp);
        assert_eq!(cfg.ip, None);
        assert_eq!(cfg.mask, None);
        assert_eq!(cfg.gateway, None);
        assert!(cfg.dns.is_empty());
    }

    // Hypothetical fully-static config with gateway and a static single DNS entry
    // (format documented by Microsoft; not distinguishable in structure from the
    // DHCP+gateway case above other than the "DHCP enabled" value and key names).
    const STATIC_WITH_GATEWAY_AND_DNS: &str = "\r\nConfiguration for interface \"Ethernet 2\"\r\n\
    DHCP enabled:                         No\r\n\
    IP Address:                           192.168.1.250\r\n\
    Subnet Prefix:                        192.168.1.0/24 (mask 255.255.255.0)\r\n\
    Default Gateway:                      192.168.1.1\r\n\
    Gateway Metric:                       0\r\n\
    InterfaceMetric:                      25\r\n\
    Statically Configured DNS Servers:    8.8.8.8\r\n\
    Register with which suffix:           Primary only\r\n\
    Statically Configured WINS Servers:   None\r\n";

    #[test]
    fn parses_fully_static_interface_with_gateway_and_dns() {
        let cfg = parse_show_config(STATIC_WITH_GATEWAY_AND_DNS, "Ethernet 2").unwrap();
        assert!(!cfg.dhcp);
        assert_eq!(cfg.ip.as_deref(), Some("192.168.1.250"));
        assert_eq!(cfg.mask.as_deref(), Some("255.255.255.0"));
        assert_eq!(cfg.gateway.as_deref(), Some("192.168.1.1"));
        assert_eq!(cfg.dns, vec!["8.8.8.8"]);
    }

    #[test]
    fn extract_mask_handles_typical_subnet_prefix() {
        assert_eq!(
            extract_mask("172.20.10.0/28 (mask 255.255.255.240)").as_deref(),
            Some("255.255.255.240")
        );
    }
}
