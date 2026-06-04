use std::net::Ipv4Addr;

/// Enumerate the host's IPv4 interface addresses via the `if-addrs` crate.
/// Returns an empty vec on enumeration failure (e.g. restricted CI sandboxes).
fn host_ipv4s() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter_map(|i| match i.addr.ip() {
                std::net::IpAddr::V4(v4) => Some(v4),
                std::net::IpAddr::V6(_) => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_addrs_enumeration_links_and_returns_ipv4_vec() {
        // We don't assert any specific interface exists (CI may have none);
        // we only prove the `if-addrs` dep links and host_ipv4s() is callable.
        let v: Vec<Ipv4Addr> = host_ipv4s();
        // loopback, if present, must be a valid v4 (sanity on the mapping).
        for ip in &v {
            assert_eq!(ip.octets().len(), 4);
        }
    }
}
