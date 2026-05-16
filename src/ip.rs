use std::net::{IpAddr, Ipv6Addr};

pub fn mask_ip_for_public(input: &str) -> Option<String> {
    let ip = input.trim().parse::<IpAddr>().ok()?;
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            Some(format!("{}.{}.{}.*", octets[0], octets[1], octets[2]))
        }
        IpAddr::V6(ipv6) => {
            if let Some(mapped) = ipv4_mapped(ipv6) {
                let octets = mapped.octets();
                return Some(format!("{}.{}.{}.*", octets[0], octets[1], octets[2]));
            }

            let segments = ipv6.segments();
            Some(format!(
                "{:x}:{:x}:{:x}::*",
                segments[0], segments[1], segments[2]
            ))
        }
    }
}

fn ipv4_mapped(ipv6: Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    ipv6.to_ipv4_mapped()
}

#[cfg(test)]
mod tests {
    use super::mask_ip_for_public;

    #[test]
    fn mask_ip_should_keep_ipv4_24_prefix() {
        assert_eq!(mask_ip_for_public("1.2.3.4"), Some("1.2.3.*".to_string()));
    }

    #[test]
    fn mask_ip_should_keep_ipv6_48_prefix_with_compact_suffix() {
        assert_eq!(
            mask_ip_for_public("2001:db8:abcd:1234::1"),
            Some("2001:db8:abcd::*".to_string())
        );
    }

    #[test]
    fn mask_ip_should_convert_ipv4_mapped_ipv6_to_ipv4_mask() {
        assert_eq!(
            mask_ip_for_public("::ffff:192.0.2.33"),
            Some("192.0.2.*".to_string())
        );
    }

    #[test]
    fn mask_ip_should_hide_invalid_input() {
        assert_eq!(mask_ip_for_public("not-an-ip"), None);
    }
}
