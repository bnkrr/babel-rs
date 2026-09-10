//! Shared wire primitives for addresses, prefixes and TLV fields.

use super::*;

pub(super) fn require_nonzero(value: u16, type_: u8) -> Result<(), WireError> {
    if value == 0 {
        Err(WireError::InvalidTlv { type_ })
    } else {
        Ok(())
    }
}

pub(super) fn require_zeroes(value: &[u8], type_: u8) -> Result<(), WireError> {
    if value.iter().any(|byte| *byte != 0) {
        Err(WireError::InvalidTlv { type_ })
    } else {
        Ok(())
    }
}

pub(super) fn put_tlv(out: &mut Vec<u8>, type_: u8, value: &[u8]) -> Result<(), WireError> {
    if value.len() > u8::MAX as usize {
        return Err(WireError::BodyTooLarge);
    }
    out.extend([type_, value.len() as u8]);
    out.extend(value);
    Ok(())
}

pub(super) fn decode_address(ae: AddressEncoding, data: &[u8]) -> Option<IpAddr> {
    match ae {
        AddressEncoding::Ipv4 => Some(IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(data).ok()?))),
        AddressEncoding::Ipv6 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(data).ok()?))),
        AddressEncoding::Ipv6LinkLocal => {
            let mut raw = [0u8; 16];
            raw[0] = 0xfe;
            raw[1] = 0x80;
            raw[8..].copy_from_slice(data);
            Some(IpAddr::V6(Ipv6Addr::from(raw)))
        }
        _ => None,
    }
}

pub(super) fn encode_address(address: IpAddr) -> (AddressEncoding, Vec<u8>) {
    match address {
        IpAddr::V4(value) => (AddressEncoding::Ipv4, value.octets().to_vec()),
        IpAddr::V6(value) if value.is_unicast_link_local() => {
            (AddressEncoding::Ipv6LinkLocal, value.octets()[8..].to_vec())
        }
        IpAddr::V6(value) => (AddressEncoding::Ipv6, value.octets().to_vec()),
    }
}

pub(super) fn encode_optional_address(address: Option<IpAddr>) -> (AddressEncoding, Vec<u8>) {
    address
        .map(encode_address)
        .unwrap_or((AddressEncoding::Wildcard, vec![]))
}

pub(super) fn prefix_encoded_len(ae: AddressEncoding, plen: u8) -> Option<usize> {
    if plen > ae.max_prefix_bits()? {
        return None;
    }
    let octets = usize::from(plen).div_ceil(8);
    match ae {
        AddressEncoding::Ipv6LinkLocal if plen >= 64 => Some(octets - 8),
        AddressEncoding::Ipv6LinkLocal => None,
        AddressEncoding::Wildcard if plen == 0 => Some(0),
        AddressEncoding::Wildcard => None,
        _ => Some(octets),
    }
}

pub(super) fn decode_prefix(
    ae: AddressEncoding,
    plen: u8,
    omitted: usize,
    suffix: &[u8],
    previous: Option<&Vec<u8>>,
) -> Option<IpNet> {
    let encoded_len = prefix_encoded_len(ae, plen)?;
    if omitted > encoded_len || suffix.len() < encoded_len - omitted {
        return None;
    }
    let mut encoded = vec![0u8; encoded_len];
    if omitted > 0 {
        encoded[..omitted].copy_from_slice(previous?.get(..omitted)?);
    }
    encoded[omitted..].copy_from_slice(&suffix[..encoded_len - omitted]);
    if !plen.is_multiple_of(8) && !encoded.is_empty() {
        let mask = 0xff << (8 - plen % 8);
        *encoded.last_mut()? &= mask;
    }
    match ae {
        AddressEncoding::Ipv4 | AddressEncoding::Ipv4ViaIpv6 => {
            let mut raw = [0; 4];
            raw[..encoded.len()].copy_from_slice(&encoded);
            Some(IpNet::V4(Ipv4Net::new(Ipv4Addr::from(raw), plen).ok()?))
        }
        AddressEncoding::Ipv6 => {
            let mut raw = [0; 16];
            raw[..encoded.len()].copy_from_slice(&encoded);
            Some(IpNet::V6(Ipv6Net::new(Ipv6Addr::from(raw), plen).ok()?))
        }
        AddressEncoding::Ipv6LinkLocal => {
            let mut raw = [0; 16];
            raw[0] = 0xfe;
            raw[1] = 0x80;
            raw[8..8 + encoded.len()].copy_from_slice(&encoded);
            Some(IpNet::V6(Ipv6Net::new(Ipv6Addr::from(raw), plen).ok()?))
        }
        AddressEncoding::Wildcard => None,
    }
}

pub(super) fn prefix_wire_bytes(prefix: IpNet, ae: AddressEncoding) -> Vec<u8> {
    let size = prefix_encoded_len(ae, prefix.prefix_len()).unwrap_or(0);
    match (prefix, ae) {
        (IpNet::V4(value), _) => value.addr().octets()[..size].to_vec(),
        (IpNet::V6(value), AddressEncoding::Ipv6LinkLocal) => {
            value.addr().octets()[8..8 + size].to_vec()
        }
        (IpNet::V6(value), _) => value.addr().octets()[..size].to_vec(),
    }
}

pub(super) fn ae_for_key(key: &RouteKey, v4_via_v6: bool) -> AddressEncoding {
    if key.destination.addr().is_ipv4() {
        if v4_via_v6 {
            AddressEncoding::Ipv4ViaIpv6
        } else {
            AddressEncoding::Ipv4
        }
    } else {
        AddressEncoding::Ipv6
    }
}

pub(super) fn router_id_from_prefix(prefix: IpNet) -> [u8; 8] {
    let bytes: Vec<u8> = match prefix {
        IpNet::V4(v) => v.addr().octets().to_vec(),
        IpNet::V6(v) => v.addr().octets().to_vec(),
    };
    let mut id = [0; 8];
    let count = bytes.len().min(8);
    id[8 - count..].copy_from_slice(&bytes[bytes.len() - count..]);
    id
}

pub(super) fn be16(data: &[u8]) -> u16 {
    u16::from_be_bytes([data[0], data[1]])
}
pub(super) fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}
