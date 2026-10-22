//! The LAN addresses the phone can reach this PC on.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

fn usable(ip: &Ipv4Addr) -> bool {
    !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified())
}

/// Address of the interface used for the default route (no packets are sent).
fn primary_ip() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?; // TEST-NET address; UDP connect only picks a route
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if usable(&ip) => Some(ip),
        _ => None,
    }
}

/// All usable IPv4 addresses: private (home/office LAN) ranges first, the
/// default-route interface first among equals.
pub fn lan_ips() -> Vec<IpAddr> {
    let primary = primary_ip();
    let mut found: Vec<Ipv4Addr> = primary.into_iter().collect();
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let IpAddr::V4(ip) = iface.ip() {
            if usable(&ip) && !found.contains(&ip) {
                found.push(ip);
            }
        }
    }
    found.sort_by_key(|ip| (!ip.is_private(), Some(*ip) != primary));
    found.into_iter().map(IpAddr::V4).collect()
}
