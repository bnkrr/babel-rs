//! RFC 8967 packet authentication. Call `receive` before the ordinary decoder;
//! reserve `overhead` when packetizing and call `sign` after timestamp stamping.
//! Each interface owns one session. Hosts supply a cryptographic random source,
//! monotonic milliseconds and the actual UDP endpoints (including destination).
//! RFC 9467 §3.1 separates unicast/multicast receive counters; no replay window.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use blake2::{Blake2sMac, digest::consts::U16};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

const MAC: u8 = 16;
const PC: u8 = 17;
const REQUEST: u8 = 18;
const REPLY: u8 = 19;
const RATE_MS: u64 = 300;
const CHALLENGE_MS: u64 = 30_000;
const EXPIRE_MS: u64 = 300_000;

/// Supported RFC 8967 algorithms. Digests are never truncated after computation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacAlgorithm {
    HmacSha256,
    Blake2s128,
}

/// A shared secret. Debug output is redacted; owned key bytes are zeroed on drop.
#[derive(Clone, Eq, PartialEq)]
pub struct MacKey {
    algorithm: MacAlgorithm,
    bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for MacKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacKey")
            .field("algorithm", &self.algorithm)
            .finish_non_exhaustive()
    }
}

impl MacKey {
    /// HMAC accepts 1–4096 bytes; keyed BLAKE2s accepts 1–32 bytes.
    /// Use random 32-byte keys for either algorithm (RFC 8967 §7).
    pub fn new(algorithm: MacAlgorithm, bytes: Vec<u8>) -> Result<Self, MacError> {
        let bytes = Zeroizing::new(bytes);
        if bytes.is_empty()
            || bytes.len() > 4096
            || (algorithm == MacAlgorithm::Blake2s128 && bytes.len() > 32)
        {
            return Err(MacError::KeyLength);
        }
        Ok(Self { algorithm, bytes })
    }

    fn digest(&self, data: &[u8]) -> Vec<u8> {
        match self.algorithm {
            MacAlgorithm::HmacSha256 => {
                let mut mac = Hmac::<Sha256>::new_from_slice(&self.bytes).expect("validated key");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }
            MacAlgorithm::Blake2s128 => {
                let mut mac =
                    <Blake2sMac<U16> as Mac>::new_from_slice(&self.bytes).expect("validated key");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }
        }
    }
}

/// Immutable interface keys, including all keys during a rotation overlap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacConfig {
    keys: Vec<MacKey>,
    accept_unverified: bool,
}

impl MacConfig {
    /// Strict authentication with 1–8 keys. Every key signs each outgoing packet.
    pub fn new(keys: Vec<MacKey>) -> Result<Self, MacError> {
        if keys.is_empty() || keys.len() > 8 {
            return Err(MacError::KeyCount);
        }
        Ok(Self {
            keys,
            accept_unverified: false,
        })
    }

    /// Explicit RFC 8967 §5 migration mode: sign output but bypass input checks.
    /// This mode provides no authentication of received routing information.
    pub fn accept_unverified(mut self, enabled: bool) -> Self {
        self.accept_unverified = enabled;
        self
    }

    pub fn is_strict(&self) -> bool {
        !self.accept_unverified
    }

    pub fn overhead(&self) -> usize {
        22 + self
            .keys
            .iter()
            .map(|k| match k.algorithm {
                MacAlgorithm::HmacSha256 => 34,
                MacAlgorithm::Blake2s128 => 18,
            })
            .sum::<usize>()
    }
}

#[derive(Debug, Error)]
pub enum MacError {
    #[error("invalid MAC key length")]
    KeyLength,
    #[error("MAC requires between one and eight keys")]
    KeyCount,
    #[error("invalid Babel authentication packet or UDP endpoints")]
    Packet,
    #[error("cryptographic randomness is unavailable")]
    Entropy,
}

/// IP addresses and UDP ports as transmitted, not a guessed multicast address.
#[derive(Clone, Copy, Debug)]
pub struct DatagramEndpoints {
    pub source: SocketAddr,
    pub destination: SocketAddr,
}

impl DatagramEndpoints {
    fn authenticated_bytes(self, packet: &[u8]) -> Result<Vec<u8>, MacError> {
        let mut result = Vec::with_capacity(36 + packet.len());
        match (self.source.ip(), self.destination.ip()) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                result.extend(s.octets());
                result.extend(self.source.port().to_be_bytes());
                result.extend(d.octets());
            }
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                result.extend(s.octets());
                result.extend(self.source.port().to_be_bytes());
                result.extend(d.octets());
            }
            _ => return Err(MacError::Packet),
        }
        result.extend(self.destination.port().to_be_bytes());
        result.extend(packet);
        Ok(result)
    }
}

#[derive(Default)]
struct Peer {
    index: Option<Vec<u8>>,
    unicast: u32,
    multicast: u32,
    nonce: Option<([u8; 16], u64)>,
    last_reply: Option<u64>,
    // Only creation or acceptance extends lifetime, never a failed challenge.
    expires: u64,
}

/// Authentication decision and already framed, unsigned unicast control packets.
/// Sign and send controls promptly to the input source, even if `accepted` is false.
#[derive(Debug, Default)]
pub struct MacReception {
    pub accepted: bool,
    pub controls: Vec<Vec<u8>>,
}

/// Bounded per-interface authentication state. Never clone or reuse a session's
/// outgoing (Index, PC). A newly created session must receive fresh randomness.
pub struct MacSession {
    config: MacConfig,
    index: [u8; 16],
    counter: u32,
    peers: BTreeMap<SocketAddr, Peer>,
    max_peers: usize,
    last_challenge: Option<u64>,
}

impl MacSession {
    pub fn new(
        config: MacConfig,
        max_peers: usize,
        random: &mut impl FnMut() -> Result<[u8; 16], MacError>,
    ) -> Result<Self, MacError> {
        Ok(Self {
            config,
            index: random()?,
            counter: 0,
            peers: BTreeMap::new(),
            max_peers,
            last_challenge: None,
        })
    }

    pub fn overhead(&self) -> usize {
        self.config.overhead()
    }
    pub fn is_strict(&self) -> bool {
        self.config.is_strict()
    }

    /// Frame one packet with exactly one PC and a MAC trailer for every key.
    /// The input must be a complete unsigned packet without authentication TLVs.
    /// Counter allocation survives cancellation; a skipped PC is harmless.
    pub fn sign(
        &mut self,
        packet: &[u8],
        endpoints: DatagramEndpoints,
        random: &mut impl FnMut() -> Result<[u8; 16], MacError>,
    ) -> Result<Vec<u8>, MacError> {
        let end = body_end(packet)?;
        if end != packet.len()
            || tlvs(&packet[4..end])?
                .iter()
                .any(|(t, _)| *t == PC || *t == MAC)
        {
            return Err(MacError::Packet);
        }
        if self.counter == u32::MAX {
            self.index = random()?;
            self.counter = 0;
        }
        let mut out = packet.to_vec();
        out.extend([PC, 20]);
        out.extend(self.counter.to_be_bytes());
        out.extend(self.index);
        self.counter += 1;
        let len = u16::try_from(out.len() - 4).map_err(|_| MacError::Packet)?;
        out[2..4].copy_from_slice(&len.to_be_bytes());
        let data = endpoints.authenticated_bytes(&out)?;
        for key in &self.config.keys {
            let digest = key.digest(&data);
            out.extend([MAC, digest.len() as u8]);
            out.extend(digest);
        }
        Ok(out)
    }

    pub fn expire(&mut self, now_ms: u64) {
        self.peers.retain(|_, p| now_ms < p.expires);
    }

    /// Invalid input never returns an error to the peer. MAC verification occurs
    /// before allocating peer state. `random` must generate fresh CSPRNG bytes.
    pub fn receive(
        &mut self,
        packet: &[u8],
        endpoints: DatagramEndpoints,
        now_ms: u64,
        random: &mut impl FnMut() -> Result<[u8; 16], MacError>,
    ) -> MacReception {
        self.expire(now_ms);
        self.receive_inner(packet, endpoints, now_ms, random)
            .unwrap_or_default()
    }

    fn receive_inner(
        &mut self,
        packet: &[u8],
        endpoints: DatagramEndpoints,
        now: u64,
        random: &mut impl FnMut() -> Result<[u8; 16], MacError>,
    ) -> Result<MacReception, MacError> {
        let end = body_end(packet)?;
        let mut result = MacReception {
            accepted: self.config.accept_unverified,
            controls: Vec::new(),
        };
        if !self.config.accept_unverified {
            let trailer = tlvs(&packet[end..])?;
            if !trailer.iter().any(|(t, _)| *t == MAC) {
                return Ok(result);
            }
            let data = endpoints.authenticated_bytes(&packet[..end])?;
            // Compute once per local key, regardless of the number of received MACs.
            let mut valid = false;
            for key in &self.config.keys {
                let digest = key.digest(&data);
                for (type_, value) in &trailer {
                    if *type_ == MAC && constant_time_eq(&digest, value) {
                        valid = true;
                    }
                }
            }
            if !valid {
                return Ok(result);
            }
        }
        let body = tlvs(&packet[4..end])?;
        if !self.peers.contains_key(&endpoints.source) && self.peers.len() >= self.max_peers {
            return Ok(result);
        }
        let peer = self.peers.entry(endpoints.source).or_insert_with(|| Peer {
            expires: now.saturating_add(EXPIRE_MS),
            ..Peer::default()
        });
        let mut counter = None;
        let mut request = None;
        let mut success = false;
        for (t, v) in body {
            match t {
                PC if counter.is_none() && (4..=36).contains(&v.len()) => {
                    counter = Some((
                        u32::from_be_bytes(v[..4].try_into().expect("checked length")),
                        &v[4..],
                    ));
                }
                REQUEST if v.len() <= 192 && !endpoints.destination.ip().is_multicast() => {
                    request = Some(v)
                }
                REPLY if v.len() <= 192 => {
                    success |= peer.nonce.is_some_and(|(nonce, expires)| {
                        now < expires && constant_time_eq(&nonce, v)
                    });
                }
                _ => {}
            }
        }
        if let Some(nonce) = request
            && peer
                .last_reply
                .is_none_or(|last| now.saturating_sub(last) >= RATE_MS)
        {
            peer.last_reply = Some(now);
            result.controls.push(control(REPLY, nonce));
        }
        let Some((pc, index)) = counter else {
            return Ok(result);
        };
        if success {
            peer.nonce = None;
            peer.index = Some(index.to_vec());
            peer.unicast = pc;
            peer.multicast = pc;
        } else if peer.index.as_deref() != Some(index) {
            if self
                .last_challenge
                .is_none_or(|last| now.saturating_sub(last) >= RATE_MS)
            {
                let nonce = random()?;
                peer.nonce = Some((nonce, now.saturating_add(CHALLENGE_MS)));
                self.last_challenge = Some(now);
                result.controls.push(control(REQUEST, &nonce));
            }
            return Ok(result);
        } else {
            let previous = if endpoints.destination.ip().is_multicast() {
                &mut peer.multicast
            } else {
                &mut peer.unicast
            };
            if pc <= *previous {
                return Ok(result);
            }
            *previous = pc;
        }
        peer.expires = now.saturating_add(EXPIRE_MS);
        result.accepted = true;
        Ok(result)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(left.ct_eq(right))
}

fn body_end(packet: &[u8]) -> Result<usize, MacError> {
    if packet.len() < 4 || packet[..2] != [42, 2] {
        return Err(MacError::Packet);
    }
    let end = 4 + usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if end > packet.len() {
        return Err(MacError::Packet);
    }
    Ok(end)
}

fn tlvs(mut bytes: &[u8]) -> Result<Vec<(u8, &[u8])>, MacError> {
    let mut out = Vec::new();
    while let Some((&t, rest)) = bytes.split_first() {
        if t == 0 {
            bytes = rest;
            continue;
        }
        let (&len, rest) = rest.split_first().ok_or(MacError::Packet)?;
        let len = usize::from(len);
        if len > rest.len() {
            return Err(MacError::Packet);
        }
        out.push((t, &rest[..len]));
        bytes = &rest[len..];
    }
    Ok(out)
}

fn control(type_: u8, nonce: &[u8]) -> Vec<u8> {
    let mut bytes = vec![42, 2];
    bytes.extend(((nonce.len() + 2) as u16).to_be_bytes());
    bytes.extend([type_, nonce.len() as u8]);
    bytes.extend(nonce);
    bytes
}

#[cfg(test)]
mod tests;
