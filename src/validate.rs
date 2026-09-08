//! Pure validation for IPv4 addresses, subnet masks and gateways.
//! Kept dependency-free and side-effect-free so it can run in unit tests
//! without touching a real network adapter.

pub fn parse_ipv4(s: &str) -> Result<[u8; 4], String> {
    let s = s.trim();
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return Err(format!("'{s}' is not a valid IPv4 address (expected 4 octets)"));
    }
    let mut out = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("'{s}' is not a valid IPv4 address (bad octet '{part}')"));
        }
        out[i] = part
            .parse::<u16>()
            .ok()
            .filter(|v| *v <= 255)
            .ok_or_else(|| format!("'{s}' is not a valid IPv4 address (octet '{part}' out of range)"))?
            as u8;
    }
    Ok(out)
}

/// A valid IPv4 netmask is a contiguous run of 1 bits followed by 0 bits
/// (e.g. 255.255.255.0), when read as a 32-bit integer.
pub fn parse_mask(s: &str) -> Result<[u8; 4], String> {
    let octets = parse_ipv4(s).map_err(|_| format!("'{s}' is not a valid subnet mask"))?;
    let value = u32::from_be_bytes(octets);
    let ones = value.leading_ones();
    let expected = if ones == 0 {
        0
    } else if ones == 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - ones)
    };
    if value != expected {
        return Err(format!("'{s}' is not a valid subnet mask (bits must be contiguous)"));
    }
    Ok(octets)
}

pub fn mask_prefix_len(mask: [u8; 4]) -> u32 {
    u32::from_be_bytes(mask).count_ones()
}

fn network_addr(ip: [u8; 4], mask: [u8; 4]) -> [u8; 4] {
    let ip = u32::from_be_bytes(ip);
    let mask = u32::from_be_bytes(mask);
    (ip & mask).to_be_bytes()
}

/// Validates a complete static config: IP/mask must parse, and if a gateway
/// is given it must fall inside the resulting subnet (a very common misconfig).
pub fn validate_static_config(ip: &str, mask: &str, gateway: &str) -> Result<(), String> {
    let ip_octets = parse_ipv4(ip).map_err(|e| format!("IP address: {e}"))?;
    let mask_octets = parse_mask(mask).map_err(|e| format!("Subnet mask: {e}"))?;

    let gateway = gateway.trim();
    if !gateway.is_empty() {
        let gw_octets = parse_ipv4(gateway).map_err(|e| format!("Gateway: {e}"))?;
        if network_addr(ip_octets, mask_octets) != network_addr(gw_octets, mask_octets) {
            return Err(format!(
                "Gateway {gateway} is not inside the {ip}/{} subnet",
                mask_prefix_len(mask_octets)
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_ip() {
        assert_eq!(parse_ipv4("192.168.1.10").unwrap(), [192, 168, 1, 10]);
    }

    #[test]
    fn rejects_ip_with_too_few_octets() {
        assert!(parse_ipv4("192.168.1").is_err());
    }

    #[test]
    fn rejects_ip_with_out_of_range_octet() {
        assert!(parse_ipv4("192.168.1.999").is_err());
    }

    #[test]
    fn rejects_ip_with_non_numeric_octet() {
        assert!(parse_ipv4("192.168.1.abc").is_err());
    }

    #[test]
    fn accepts_common_masks() {
        assert!(parse_mask("255.255.255.0").is_ok());
        assert!(parse_mask("255.255.0.0").is_ok());
        assert!(parse_mask("255.0.0.0").is_ok());
        assert!(parse_mask("255.255.255.252").is_ok());
        assert!(parse_mask("255.255.255.255").is_ok());
        assert!(parse_mask("0.0.0.0").is_ok());
    }

    #[test]
    fn rejects_non_contiguous_mask() {
        // 255.255.0.255 -> bits are 1s, 0s, then 1s again: not contiguous.
        assert!(parse_mask("255.255.0.255").is_err());
        assert!(parse_mask("255.0.255.0").is_err());
    }

    #[test]
    fn prefix_len_matches_mask() {
        assert_eq!(mask_prefix_len(parse_mask("255.255.255.0").unwrap()), 24);
        assert_eq!(mask_prefix_len(parse_mask("255.255.0.0").unwrap()), 16);
    }

    #[test]
    fn gateway_in_subnet_is_valid() {
        assert!(validate_static_config("192.168.1.250", "255.255.255.0", "192.168.1.1").is_ok());
    }

    #[test]
    fn gateway_outside_subnet_is_rejected() {
        let err = validate_static_config("192.168.1.250", "255.255.255.0", "192.168.2.1").unwrap_err();
        assert!(err.contains("not inside"), "unexpected error: {err}");
    }

    #[test]
    fn empty_gateway_is_allowed() {
        assert!(validate_static_config("192.168.1.250", "255.255.255.0", "").is_ok());
    }

    #[test]
    fn invalid_ip_is_rejected_before_gateway_check() {
        assert!(validate_static_config("999.1.1.1", "255.255.255.0", "").is_err());
    }
}
