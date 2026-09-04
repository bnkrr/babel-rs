use std::fmt;
use std::net::IpAddr;

use ipnet::IpNet;

pub const INFINITY: u16 = 0xffff;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RouterId([u8; 8]);

impl RouterId {
    pub fn new(value: [u8; 8]) -> Option<Self> {
        if value == [0; 8] || value == [0xff; 8] {
            None
        } else {
            Some(Self(value))
        }
    }

    pub const fn octets(self) -> [u8; 8] {
        self.0
    }
}

impl fmt::Display for RouterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, value) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str(":")?;
            }
            write!(f, "{value:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RouteKey {
    pub destination: IpNet,
    pub source: Option<IpNet>,
}

impl RouteKey {
    pub fn new(destination: IpNet, source: Option<IpNet>) -> Option<Self> {
        if source.is_some_and(|value| value.addr().is_ipv4() != destination.addr().is_ipv4()) {
            return None;
        }
        // RFC 9079 represents a zero-length source prefix by omitting the
        // Source Prefix sub-TLV altogether.
        let source = source.filter(|value| value.prefix_len() != 0);
        Some(Self {
            destination: destination.trunc(),
            source: source.map(|value| value.trunc()),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Distance {
    pub seqno: u16,
    pub metric: u16,
}

impl Distance {
    pub fn feasible_against(self, feasible: Self) -> bool {
        seqno_gt(self.seqno, feasible.seqno)
            || (self.seqno == feasible.seqno && self.metric < feasible.metric)
    }
}

// RFC 8966 sequence-number comparison modulo 2^16. Values exactly half a
// sequence space apart are deliberately incomparable.
pub fn seqno_gt(a: u16, b: u16) -> bool {
    let delta = a.wrapping_sub(b);
    delta != 0 && delta < 0x8000
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedRoute {
    pub key: RouteKey,
    pub router_id: RouterId,
    pub seqno: u16,
    pub metric: u16,
    pub next_hop: IpAddr,
    pub interface: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_numbers_wrap() {
        assert!(seqno_gt(0, 65535));
        assert!(seqno_gt(20, 10));
        assert!(!seqno_gt(10, 20));
        assert!(!seqno_gt(0x8000, 0));
    }

    #[test]
    fn feasibility_uses_advertised_metric() {
        let fd = Distance {
            seqno: 7,
            metric: 100,
        };
        assert!(
            Distance {
                seqno: 8,
                metric: 500
            }
            .feasible_against(fd)
        );
        assert!(
            Distance {
                seqno: 7,
                metric: 99
            }
            .feasible_against(fd)
        );
        assert!(
            !Distance {
                seqno: 7,
                metric: 100
            }
            .feasible_against(fd)
        );
    }
}
