use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::Path;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use babel_proto::wire::PORT;

pub struct InterfaceSocket {
    pub name: String,
    pub index: u32,
    pub local_addresses: Vec<Ipv6Addr>,
    pub socket: UdpSocket,
}

impl InterfaceSocket {
    pub fn open(name: &str) -> io::Result<Self> {
        let index = interface_index(name)?;
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
        socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(socket.into())?;
        Ok(Self {
            name: name.to_owned(),
            index,
            local_addresses,
            socket,
        })
    }

    pub fn destination(&self, address: Ipv6Addr) -> SocketAddr {
        SocketAddr::V6(SocketAddrV6::new(address, PORT, 0, self.index))
    }
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
