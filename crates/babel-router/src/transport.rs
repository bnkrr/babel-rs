use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::Path;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use babel_proto::wire::PORT;

const IPV6_HEADER_BYTES: u32 = 40;
const UDP_HEADER_BYTES: u32 = 8;
const IPV6_MINIMUM_MTU: u32 = 1280;
const MAX_UDP_PAYLOAD: usize = 65_527;

pub struct InterfaceSocket {
    pub name: String,
    pub index: u32,
    pub local_addresses: Vec<Ipv6Addr>,
    pub mtu: u32,
    pub socket: UdpSocket,
}

impl InterfaceSocket {
    pub fn open(name: &str) -> io::Result<Self> {
        let index = interface_index(name)?;
        let mtu = interface_mtu(name)?;
        payload_budget_for_mtu(mtu)?;
        let local_addresses = interface_ipv6_addresses(name)?;
        if !local_addresses.iter().any(Ipv6Addr::is_unicast_link_local) {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("interface {name} has no IPv6 link-local address"),
            ));
        }
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(true)?;
        socket.set_reuse_address(true)?;
        #[cfg(target_os = "linux")]
        socket.bind_device(Some(name.as_bytes()))?;
        socket.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, PORT, 0, 0).into())?;
        socket.join_multicast_v6(&Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 6), index)?;
        socket.set_multicast_if_v6(index)?;
        socket.set_multicast_hops_v6(1)?;
        socket.set_unicast_hops_v6(1)?;
        socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(socket.into())?;
        Ok(Self {
            name: name.to_owned(),
            index,
            local_addresses,
            mtu,
            socket,
        })
    }

    pub fn destination(&self, address: Ipv6Addr) -> SocketAddr {
        SocketAddr::V6(SocketAddrV6::new(address, PORT, 0, self.index))
    }

    /// Return the current interface MTU, not merely the value observed when
    /// the socket was opened.  Linux exposes MTU changes atomically through
    /// sysfs, which keeps packetisation correct without restarting a session.
    pub fn current_mtu(&self) -> io::Result<u32> {
        interface_mtu(&self.name)
    }

    pub fn payload_budget(&self) -> io::Result<usize> {
        payload_budget_for_mtu(self.current_mtu()?)
    }
}

pub(crate) fn payload_budget_for_mtu(mtu: u32) -> io::Result<usize> {
    if mtu < IPV6_MINIMUM_MTU {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("IPv6 interface MTU {mtu} is below the required minimum {IPV6_MINIMUM_MTU}"),
        ));
    }
    Ok(usize::try_from(mtu - IPV6_HEADER_BYTES - UDP_HEADER_BYTES)
        .unwrap_or(MAX_UDP_PAYLOAD)
        .min(MAX_UDP_PAYLOAD))
}

fn interface_ipv6_addresses(name: &str) -> io::Result<Vec<Ipv6Addr>> {
    let contents = std::fs::read_to_string("/proc/net/if_inet6")?;
    let mut addresses = Vec::new();
    for line in contents.lines() {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        if fields.len() != 6 || fields[5] != name || fields[0].len() != 32 {
            continue;
        }
        let mut raw = [0u8; 16];
        let mut valid = true;
        for (index, byte) in raw.iter_mut().enumerate() {
            match u8::from_str_radix(&fields[0][index * 2..index * 2 + 2], 16) {
                Ok(value) => *byte = value,
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if valid {
            addresses.push(Ipv6Addr::from(raw));
        }
    }
    Ok(addresses)
}

fn interface_index(name: &str) -> io::Result<u32> {
    let value = std::fs::read_to_string(Path::new("/sys/class/net").join(name).join("ifindex"))?;
    value
        .trim()
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn interface_mtu(name: &str) -> io::Result<u32> {
    let value = std::fs::read_to_string(Path::new("/sys/class/net").join(name).join("mtu"))?;
    value
        .trim()
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_ipv6_udp_payload_budget() {
        assert_eq!(payload_budget_for_mtu(1280).unwrap(), 1232);
        assert_eq!(payload_budget_for_mtu(1500).unwrap(), 1452);
        assert!(payload_budget_for_mtu(1279).is_err());
        assert_eq!(payload_budget_for_mtu(u32::MAX).unwrap(), MAX_UDP_PAYLOAD);
    }
}
