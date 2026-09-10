use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::Path;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use babel_protocol::mac::{DatagramEndpoints, MacConfig, MacError, MacReception, MacSession};
use babel_protocol::{ControlTransport, wire::PORT};

const IPV6_HEADER_BYTES: u32 = 40;
const UDP_HEADER_BYTES: u32 = 8;
const IPV6_MINIMUM_MTU: u32 = 1280;
const MAX_UDP_PAYLOAD: usize = 65_527;

pub struct InterfaceSocket {
    pub name: String,
    pub transport: ControlTransport,
    pub index: u32,
    pub addresses: std::sync::RwLock<Vec<IpAddr>>,
    pub mtu: u32,
    pub socket: UdpSocket,
    pub authentication: std::sync::Mutex<Option<MacSession>>,
}

impl InterfaceSocket {
    pub fn open_authenticated(
        name: &str,
        transport: ControlTransport,
        mac: Option<MacConfig>,
        max_peers: usize,
    ) -> io::Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "babel-router currently supports Linux interfaces only",
            ));
        }
        let index = interface_index(name)?;
        let mtu = interface_mtu(name)?;
        payload_budget_for_transport(mtu, transport)?;
        let addresses = interface_addresses(name)?;
        let available = addresses.iter().any(|address| match (transport, address) {
            (ControlTransport::Ipv6, IpAddr::V6(ip)) => ip.is_unicast_link_local(),
            (ControlTransport::Ipv4, IpAddr::V4(ip)) => {
                !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast()
            }
            _ => false,
        });
        if !available {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!(
                    "interface {name} has no usable {} control address",
                    transport.as_str()
                ),
            ));
        }
        let domain = match transport {
            ControlTransport::Ipv6 => Domain::IPV6,
            ControlTransport::Ipv4 => Domain::IPV4,
        };
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        #[cfg(target_os = "linux")]
        match transport {
            ControlTransport::Ipv6 => nix::sys::socket::setsockopt(
                &socket,
                nix::sys::socket::sockopt::Ipv6RecvPacketInfo,
                &true,
            )?,
            ControlTransport::Ipv4 => nix::sys::socket::setsockopt(
                &socket,
                nix::sys::socket::sockopt::Ipv4PacketInfo,
                &true,
            )?,
        }
        socket.set_reuse_address(true)?;
        #[cfg(target_os = "linux")]
        socket.bind_device(Some(name.as_bytes()))?;
        match transport {
            ControlTransport::Ipv6 => {
                socket.set_only_v6(true)?;
                socket.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, PORT, 0, 0).into())?;
                socket.join_multicast_v6(&Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 6), index)?;
                socket.set_multicast_if_v6(index)?;
                socket.set_multicast_hops_v6(1)?;
                socket.set_unicast_hops_v6(1)?;
            }
            ControlTransport::Ipv4 => {
                socket.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT).into())?;
                #[cfg(target_os = "linux")]
                socket.join_multicast_v4_n(
                    &Ipv4Addr::new(224, 0, 0, 111),
                    &socket2::InterfaceIndexOrAddress::Index(index),
                )?;
                socket.set_multicast_ttl_v4(1)?;
                socket.set_ttl_v4(1)?;
            }
        }
        socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(socket.into())?;
        Ok(Self {
            name: name.to_owned(),
            transport,
            index,
            addresses: std::sync::RwLock::new(addresses),
            mtu,
            socket,
            authentication: std::sync::Mutex::new(
                mac.map(|config| MacSession::new(config, max_peers, &mut random))
                    .transpose()
                    .map_err(io::Error::other)?,
            ),
        })
    }

    pub fn destination(&self, address: IpAddr) -> SocketAddr {
        match address {
            IpAddr::V6(ip) => SocketAddr::V6(SocketAddrV6::new(ip, PORT, 0, self.index)),
            IpAddr::V4(ip) => SocketAddr::new(ip.into(), PORT),
        }
    }

    /// Return the current interface MTU, not merely the value observed when
    /// the socket was opened.  Linux exposes MTU changes atomically through
    /// sysfs, which keeps packetisation correct without restarting a session.
    pub fn current_mtu(&self) -> io::Result<u32> {
        interface_mtu(&self.name)
    }

    pub fn payload_budget(&self) -> io::Result<usize> {
        let budget = payload_budget_for_transport(self.current_mtu()?, self.transport)?;
        let overhead = self
            .authentication
            .lock()
            .expect("MAC lock")
            .as_ref()
            .map_or(0, MacSession::overhead);
        budget
            .checked_sub(overhead)
            .filter(|b| *b >= 198)
            .ok_or_else(|| io::Error::other("MTU cannot accommodate MAC overhead"))
    }

    pub fn authenticate(
        &self,
        bytes: &[u8],
        endpoints: DatagramEndpoints,
        now_ms: u64,
    ) -> MacReception {
        self.authentication
            .lock()
            .expect("MAC lock")
            .as_mut()
            .map_or_else(
                || MacReception {
                    accepted: true,
                    controls: Vec::new(),
                },
                |mac| mac.receive(bytes, endpoints, now_ms, &mut random),
            )
    }
}

fn random() -> Result<[u8; 16], MacError> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| MacError::Entropy)?;
    Ok(bytes)
}

#[cfg(target_os = "linux")]
mod datagrams;

#[cfg(not(target_os = "linux"))]
impl InterfaceSocket {
    pub async fn receive(&self, _: &mut [u8]) -> io::Result<(usize, DatagramEndpoints)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Linux transport required",
        ))
    }
    pub async fn send(&self, _: &[u8], _: IpAddr) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Linux transport required",
        ))
    }
}

/// Read live interface addresses without shelling out or importing daemon netlink types.
pub(crate) fn interface_addresses(name: &str) -> io::Result<Vec<IpAddr>> {
    #[cfg(target_os = "linux")]
    {
        let mut addresses = Vec::new();
        for item in nix::ifaddrs::getifaddrs().map_err(io::Error::from)? {
            if item.interface_name != name {
                continue;
            }
            if let Some(address) = item.address {
                if let Some(v4) = address.as_sockaddr_in() {
                    addresses.push(IpAddr::V4(v4.ip()));
                }
                if let Some(v6) = address.as_sockaddr_in6() {
                    addresses.push(IpAddr::V6(v6.ip()));
                }
            }
        }
        addresses.sort();
        addresses.dedup();
        Ok(addresses)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = name;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "babel-router currently supports Linux interfaces only",
        ))
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

pub(crate) fn payload_budget_for_transport(
    mtu: u32,
    transport: ControlTransport,
) -> io::Result<usize> {
    match transport {
        ControlTransport::Ipv6 => payload_budget_for_mtu(mtu),
        ControlTransport::Ipv4 => Ok(mtu.saturating_sub(28).clamp(512, 65_507) as usize),
    }
}

/// Check the live receiving-interface subnet (including point-to-point peers).
/// A failed address lookup rejects the packet; it never broadens admission.
pub(crate) fn ipv4_on_link(name: &str, source: Ipv4Addr) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(addresses) = nix::ifaddrs::getifaddrs() {
        return addresses
            .filter(|item| item.interface_name == name)
            .any(|item| {
                let Some(local) = item
                    .address
                    .and_then(|a| a.as_sockaddr_in().map(|a| a.ip()))
                else {
                    return false;
                };
                let peer = item
                    .destination
                    .and_then(|a| a.as_sockaddr_in().map(|a| a.ip()));
                let mask = item
                    .netmask
                    .and_then(|a| a.as_sockaddr_in().map(|a| a.ip()));
                peer == Some(source)
                    || mask.is_some_and(|mask| {
                        u32::from(local) & u32::from(mask) == u32::from(source) & u32::from(mask)
                    })
            });
    }
    let _ = (name, source);
    false
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
