//! Bounded reusable native sessions. Each operation still owns and authorizes its stream.
use iroh::{Endpoint, EndpointAddr, endpoint::Connection};
use std::{
    collections::BTreeMap,
    ops::Deref,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{OnceCell, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
    time::Instant,
};
use wispnet_agent::tunnel::HANDSHAKE_TIMEOUT;
use wispnet_core::identity::EndpointId;

const IDLE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Protocol {
    Control,
    Http,
}

struct Entry {
    connection: OnceCell<Connection>,
    slots: Arc<Semaphore>,
    touched: Mutex<Instant>,
    closed: AtomicBool,
}
impl Entry {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.slots.close();
        if let Some(connection) = self.connection.get() {
            connection.close(0u32.into(), b"client session released");
        }
    }
    fn unusable(&self) -> bool {
        self.closed.load(Ordering::Acquire)
            || self
                .connection
                .get()
                .is_some_and(|c| c.close_reason().is_some())
    }
    fn idle(&self) -> Duration {
        self.touched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Default)]
struct State {
    entries: BTreeMap<(EndpointId, Protocol), Arc<Entry>>,
}
impl State {
    fn expire(&mut self) {
        self.entries.retain(|_, entry| {
            let expired = Arc::strong_count(entry) == 1 && entry.idle() >= IDLE;
            let retain = !(entry.unusable() || expired);
            if !retain {
                entry.close();
            }
            retain
        });
    }
}

#[derive(Default)]
pub(crate) struct Pool {
    state: Arc<Mutex<State>>,
    reaper: Mutex<Option<JoinHandle<()>>>,
}
impl Pool {
    fn start_reaper(&self) {
        let mut task = self.reaper.lock().unwrap_or_else(|e| e.into_inner());
        if task.is_some() {
            return;
        }
        let state = Arc::downgrade(&self.state);
        *task = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(state) = state.upgrade() else {
                    return;
                };
                state.lock().unwrap_or_else(|e| e.into_inner()).expire();
            }
        }));
    }

    pub(crate) async fn get(
        &self,
        endpoint: &Endpoint,
        peer: EndpointId,
        protocol: Protocol,
        capacity: usize,
        streams: usize,
        target: impl Future<Output = Result<EndpointAddr, String>>,
    ) -> Result<Lease, String> {
        self.start_reaper();
        let entry = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.expire();
            let key = (peer, protocol);
            if !state.entries.contains_key(&key) && state.entries.len() >= capacity {
                // Never evict active requests or a dial in progress to make room.
                let oldest = state
                    .entries
                    .iter()
                    .filter(|(_, e)| Arc::strong_count(e) == 1)
                    .max_by_key(|(_, e)| e.idle())
                    .map(|(key, _)| *key);
                if let Some(oldest) = oldest {
                    state.entries.remove(&oldest);
                } else {
                    return Err("peer connection pool busy".into());
                }
            }
            state
                .entries
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(Entry {
                        connection: OnceCell::new(),
                        slots: Arc::new(Semaphore::new(streams)),
                        touched: Mutex::new(Instant::now()),
                        closed: AtomicBool::new(false),
                    })
                })
                .clone()
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let permit = entry
                .slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| "peer connection invalidated")?;
            let connection = entry
                .connection
                .get_or_try_init(|| async {
                    // Reserve the entry before any asynchronous address/pairing
                    // lookup so forget() also invalidates a dial still resolving.
                    let target = target.await?;
                    if entry.closed.load(Ordering::Acquire) || target.id != peer {
                        return Err("peer connection invalidated or identity mismatch".to_owned());
                    }
                    let connection = match protocol {
                        Protocol::Control => {
                            wispnet_agent::control::connect_target(endpoint, target).await
                        }
                        Protocol::Http => {
                            wispnet_agent::tunnel::connect_target(endpoint, target).await
                        }
                    }
                    .map_err(|e| e.to_string())?;
                    if entry.closed.load(Ordering::Acquire) {
                        connection.close(0u32.into(), b"client session invalidated while dialing");
                        return Err("peer connection invalidated".to_owned());
                    }
                    Ok(connection)
                })
                .await?;
            if entry.unusable() || connection.remote_id() != peer {
                entry.close();
                return Err("peer connection closed or identity mismatch".into());
            }
            *entry.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            Ok(Lease {
                entry,
                _permit: permit,
            })
        })
        .await
        .map_err(|_| "peer connection setup timeout".to_owned())?
    }

    pub(crate) fn forget(&self, peer: Option<EndpointId>) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .retain(|(id, _), entry| {
                if peer.is_none_or(|peer| peer == *id) {
                    entry.close();
                    false
                } else {
                    true
                }
            });
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        if let Some(task) = self.reaper.lock().unwrap_or_else(|e| e.into_inner()).take() {
            task.abort();
        }
        self.forget(None);
    }
}

pub(crate) struct Lease {
    entry: Arc<Entry>,
    _permit: OwnedSemaphorePermit,
}
impl Deref for Lease {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.entry.connection.get().expect("connected session")
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        *self.entry.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::endpoint::presets;

    async fn peer() -> (Endpoint, JoinHandle<()>) {
        let endpoint = Endpoint::builder(presets::Minimal)
            .alpns(vec![
                wispnet_core::host::ALPN.to_vec(),
                wispnet_core::protocol::ALPN.to_vec(),
            ])
            .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
            .unwrap()
            .bind()
            .await
            .unwrap();
        let server = endpoint.clone();
        let worker = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    incoming = server.accept() => {
                        let Some(incoming) = incoming else { break; };
                        connections.spawn(async move {
                            if let Ok(connection) = incoming.await { connection.closed().await; }
                        });
                    }
                }
            }
        });
        (endpoint, worker)
    }
    fn target(endpoint: &Endpoint) -> EndpointAddr {
        EndpointAddr::new(endpoint.id()).with_ip_addr(endpoint.bound_sockets()[0])
    }
    async fn get(
        pool: &Pool,
        endpoint: &Endpoint,
        peer: &Endpoint,
        capacity: usize,
        streams: usize,
    ) -> Result<Lease, String> {
        pool.get(
            endpoint,
            peer.id(),
            Protocol::Control,
            capacity,
            streams,
            std::future::ready(Ok(target(peer))),
        )
        .await
    }

    #[tokio::test]
    async fn capacity_evicts_idle_sessions_without_interrupting_active_work() {
        let (first, a) = peer().await;
        let (second, b) = peer().await;
        let endpoint = Endpoint::bind(presets::Minimal).await.unwrap();
        let pool = Pool::default();
        let lease = get(&pool, &endpoint, &first, 1, 1).await.unwrap();
        let observed = (*lease).clone();
        assert!(get(&pool, &endpoint, &second, 1, 1).await.is_err());
        assert!(lease.close_reason().is_none());
        drop(lease);
        let replacement = get(&pool, &endpoint, &second, 1, 1).await.unwrap();
        assert_eq!(replacement.remote_id(), second.id());
        assert!(observed.close_reason().is_some());
        pool.forget(None);
        assert!(replacement.close_reason().is_some());
        a.abort();
        b.abort();
        endpoint.close().await;
        first.close().await;
        second.close().await;
    }

    #[tokio::test]
    async fn cancelling_a_slot_waiter_preserves_the_session_and_stream_budget() {
        let (server, worker) = peer().await;
        let endpoint = Endpoint::bind(presets::Minimal).await.unwrap();
        let pool = Pool::default();
        let lease = get(&pool, &endpoint, &server, 2, 1).await.unwrap();
        let id = lease.stable_id();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                get(&pool, &endpoint, &server, 2, 1)
            )
            .await
            .is_err()
        );
        assert!(lease.close_reason().is_none());
        drop(lease);
        let again = get(&pool, &endpoint, &server, 2, 1).await.unwrap();
        assert_eq!(again.stable_id(), id);
        drop(again);
        drop(pool);
        worker.abort();
        endpoint.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn forget_cancels_a_connection_still_resolving_its_saved_target() {
        let (server, worker) = peer().await;
        let endpoint = Endpoint::bind(presets::Minimal).await.unwrap();
        let pool = Pool::default();
        let (resolving, mut started) = tokio::sync::oneshot::channel();
        let (resume, wait) = tokio::sync::oneshot::channel();
        let dial = pool.get(&endpoint, server.id(), Protocol::Control, 2, 1, async {
            resolving.send(()).unwrap();
            wait.await.unwrap();
            Ok(target(&server))
        });
        tokio::pin!(dial);
        tokio::select! {
            result = &mut dial => panic!("dial finished before resolution: {}", result.is_ok()),
            _ = &mut started => {},
        }
        pool.forget(Some(server.id()));
        resume.send(()).unwrap();
        assert!(dial.await.is_err());
        assert!(pool.state.lock().unwrap().entries.is_empty());
        worker.abort();
        endpoint.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn protocols_use_distinct_live_sessions_without_aggregate_telemetry() {
        let (server, worker) = peer().await;
        let endpoint = Endpoint::bind(presets::Minimal).await.unwrap();
        let pool = Pool::default();
        let control = get(&pool, &endpoint, &server, 4, 2).await.unwrap();
        let http = pool
            .get(
                &endpoint,
                server.id(),
                Protocol::Http,
                4,
                2,
                std::future::ready(Ok(target(&server))),
            )
            .await
            .unwrap();
        assert_eq!(control.alpn(), wispnet_core::host::ALPN);
        assert_eq!(http.alpn(), wispnet_core::protocol::ALPN);
        assert_ne!(control.stable_id(), http.stable_id());
        let again = get(&pool, &endpoint, &server, 4, 2).await.unwrap();
        assert_eq!(control.stable_id(), again.stable_id());
        drop(pool);
        worker.abort();
        endpoint.close().await;
        server.close().await;
    }
}
