use iroh::{EndpointId, endpoint::IncomingAddr};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
    sync::{Arc, Mutex},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Source {
    Ip(IpAddr),
    Relay(EndpointId),
    Custom,
}

pub struct HandshakeLimits {
    slots: Arc<Semaphore>,
    sources: Arc<Mutex<HashMap<Source, usize>>>,
    per_source: usize,
}

impl HandshakeLimits {
    pub fn new(max: usize, per_source: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(max)),
            sources: Arc::default(),
            per_source,
        }
    }

    pub fn try_acquire(&self, address: IncomingAddr) -> Option<HandshakePermit> {
        let source = match address {
            IncomingAddr::Ip(addr) => Source::Ip(match addr.ip().to_canonical() {
                IpAddr::V4(ip) => IpAddr::V4(ip),
                IpAddr::V6(ip) => IpAddr::V6(Ipv6Addr::from(u128::from(ip) & (u128::MAX << 64))),
            }),
            IncomingAddr::Relay { endpoint_id, .. } => Source::Relay(endpoint_id),
            _ => Source::Custom,
        };
        let slot = self.slots.clone().try_acquire_owned().ok()?;
        let mut sources = self.sources.lock().unwrap();
        let count = sources.entry(source).or_default();
        if *count >= self.per_source {
            return None;
        }
        *count += 1;
        Some(HandshakePermit {
            _slot: slot,
            sources: self.sources.clone(),
            source,
        })
    }
}

pub struct HandshakePermit {
    _slot: OwnedSemaphorePermit,
    sources: Arc<Mutex<HashMap<Source, usize>>>,
    source: Source,
}

impl Drop for HandshakePermit {
    fn drop(&mut self) {
        let mut sources = self.sources.lock().unwrap();
        let count = sources
            .get_mut(&self.source)
            .expect("active handshake source");
        *count -= 1;
        if *count == 0 {
            sources.remove(&self.source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_limit_is_per_source_and_releases_capacity() {
        let limits = HandshakeLimits::new(2, 1);
        let first = IncomingAddr::Ip("127.0.0.1:1000".parse().unwrap());
        let same = IncomingAddr::Ip("[::ffff:127.0.0.1]:1001".parse().unwrap());
        let second = IncomingAddr::Ip("127.0.0.2:1000".parse().unwrap());
        let third = IncomingAddr::Ip("127.0.0.3:1000".parse().unwrap());
        let a = limits.try_acquire(first.clone()).unwrap();
        assert!(limits.try_acquire(same).is_none());
        let b = limits.try_acquire(second).unwrap();
        assert!(limits.try_acquire(third.clone()).is_none());
        drop(a);
        assert!(limits.try_acquire(third).is_some());
        drop(b);
        assert!(limits.sources.lock().unwrap().is_empty());
        assert!(limits.try_acquire(first).is_some());
    }
}
