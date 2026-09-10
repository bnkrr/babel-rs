//! Safe recvmsg/sendmsg wrappers: MAC pseudo-headers use kernel packet metadata,
//! and IP_PKTINFO/IPV6_PKTINFO pins the signed source address on transmission.
use super::*;
use nix::libc;
use nix::sys::socket::{
    ControlMessage, ControlMessageOwned, MsgFlags, SockaddrStorage, recvmsg, sendmsg,
};
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::AsRawFd;
use tokio::io::Interest;

impl InterfaceSocket {
    pub async fn receive(&self, buffer: &mut [u8]) -> io::Result<(usize, DatagramEndpoints)> {
        self.socket
            .async_io(Interest::READABLE, || {
                let mut control = nix::cmsg_space!(libc::in6_pktinfo, libc::in_pktinfo);
                let mut slices = [IoSliceMut::new(buffer)];
                let message = recvmsg::<SockaddrStorage>(
                    self.socket.as_raw_fd(),
                    &mut slices,
                    Some(&mut control),
                    MsgFlags::MSG_DONTWAIT,
                )?;
                if message
                    .flags
                    .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated UDP metadata",
                    ));
                }
                let address = message
                    .address
                    .ok_or_else(|| io::Error::other("missing UDP source"))?;
                let source = if let Some(v6) = address.as_sockaddr_in6() {
                    SocketAddr::V6(SocketAddrV6::new(
                        v6.ip(),
                        v6.port(),
                        v6.flowinfo(),
                        v6.scope_id(),
                    ))
                } else if let Some(v4) = address.as_sockaddr_in() {
                    SocketAddr::new(v4.ip().into(), v4.port())
                } else {
                    return Err(io::Error::other("invalid UDP source"));
                };
                let mut destination = None;
                for cmsg in message.cmsgs()? {
                    match cmsg {
                        ControlMessageOwned::Ipv6PacketInfo(info)
                            if info.ipi6_ifindex == self.index =>
                        {
                            destination = Some(SocketAddr::V6(SocketAddrV6::new(
                                Ipv6Addr::from(info.ipi6_addr.s6_addr),
                                PORT,
                                0,
                                self.index,
                            )));
                        }
                        ControlMessageOwned::Ipv4PacketInfo(info)
                            if info.ipi_ifindex == self.index as i32 =>
                        {
                            destination = Some(SocketAddr::new(
                                Ipv4Addr::from(info.ipi_addr.s_addr.to_ne_bytes()).into(),
                                PORT,
                            ));
                        }
                        _ => {}
                    }
                }
                Ok((
                    message.bytes,
                    DatagramEndpoints {
                        source,
                        destination: destination
                            .ok_or_else(|| io::Error::other("missing UDP destination"))?,
                    },
                ))
            })
            .await
    }

    pub async fn send(&self, packet: &[u8], destination: IpAddr) -> io::Result<usize> {
        let source = self
            .addresses
            .read()
            .expect("address lock")
            .iter()
            .copied()
            .find(|ip| match (self.transport, ip) {
                (ControlTransport::Ipv6, IpAddr::V6(v6)) => v6.is_unicast_link_local(),
                (ControlTransport::Ipv4, IpAddr::V4(v4)) => {
                    !v4.is_unspecified() && !v4.is_loopback() && !v4.is_multicast()
                }
                _ => false,
            })
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::AddrNotAvailable, "no control source address")
            })?;
        let endpoints = DatagramEndpoints {
            source: self.destination(source),
            destination: self.destination(destination),
        };
        let signed = self
            .authentication
            .lock()
            .expect("MAC lock")
            .as_mut()
            .map(|mac| mac.sign(packet, endpoints, &mut random))
            .transpose()
            .map_err(io::Error::other)?;
        let bytes = signed.as_deref().unwrap_or(packet);
        let address = SockaddrStorage::from(endpoints.destination);
        self.socket
            .async_io(Interest::WRITABLE, || {
                let slices = [IoSlice::new(bytes)];
                match source {
                    IpAddr::V6(source) => {
                        let info = libc::in6_pktinfo {
                            ipi6_addr: libc::in6_addr {
                                s6_addr: source.octets(),
                            },
                            ipi6_ifindex: self.index,
                        };
                        sendmsg(
                            self.socket.as_raw_fd(),
                            &slices,
                            &[ControlMessage::Ipv6PacketInfo(&info)],
                            MsgFlags::MSG_DONTWAIT,
                            Some(&address),
                        )
                        .map_err(io::Error::from)
                    }
                    IpAddr::V4(source) => {
                        let info = libc::in_pktinfo {
                            ipi_ifindex: self.index as i32,
                            ipi_spec_dst: libc::in_addr {
                                s_addr: u32::from_ne_bytes(source.octets()),
                            },
                            ipi_addr: libc::in_addr { s_addr: 0 },
                        };
                        sendmsg(
                            self.socket.as_raw_fd(),
                            &slices,
                            &[ControlMessage::Ipv4PacketInfo(&info)],
                            MsgFlags::MSG_DONTWAIT,
                            Some(&address),
                        )
                        .map_err(io::Error::from)
                    }
                }
            })
            .await
    }
}
