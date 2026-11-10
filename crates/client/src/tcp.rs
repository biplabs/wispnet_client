//! Owner-local TCP listeners sharing the node's identity and authorized sessions.
use crate::{Client, argument, connections::Protocol, peer};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::{JoinHandle, JoinSet},
};
use wispnet_core::{host::ServiceKind, identity::EndpointId};

#[derive(Default)]
pub(crate) struct Forwards {
    routes: Mutex<BTreeMap<(EndpointId, String), Route>>,
}
struct Route {
    incarnation: String,
    port: u16,
    task: JoinHandle<()>,
}
impl Drop for Route {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Forwards {
    pub(crate) async fn close(&self, host: Option<EndpointId>) {
        self.routes
            .lock()
            .await
            .retain(|(peer, _), _| host.is_some_and(|h| h != *peer));
    }
}
impl Client {
    // Concurrent, valid refreshes may contend on the durable baseline's
    // nonblocking file lock. Retry only that transient local error; never use
    // a cached catalog or relax rollback, generation, pairing, or grant checks.
    pub(crate) async fn connection_catalog(
        &self,
        host: EndpointId,
    ) -> Result<serde_json::Value, String> {
        let mut retries = 0;
        loop {
            match self.catalog(host).await {
                Err(error) if error == "Catalog refresh busy; retry this host" && retries < 40 => {
                    retries += 1;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                result => return result,
            }
        }
    }
    async fn tcp_permitted(
        &self,
        host: EndpointId,
        service: &str,
        incarnation: &str,
    ) -> Result<(), String> {
        if !self.gateway.is_enabled() {
            return Err("Wispnet is offline".into());
        }
        let allowed = if host == self.endpoint.id() {
            self.local_authority
                .as_ref()
                .ok_or("Local host unavailable")?
                .services()
                .await
                .map_err(|e| e.to_string())?
                .iter()
                .any(|s| {
                    s.id == service && s.incarnation == incarnation && s.kind == ServiceKind::Tcp
                })
        } else {
            if self.saved_host(host).await?.is_none() {
                return Err("Host not paired".into());
            }
            let catalog = self.connection_catalog(host).await?;
            catalog["services"].as_array().is_some_and(|items| {
                items.iter().any(|s| {
                    s["id"] == service && s["incarnation"] == incarnation && s["kind"] == "tcp"
                })
            })
        };
        if allowed {
            Ok(())
        } else {
            Err("TCP service no longer permitted".into())
        }
    }
    pub(crate) async fn forward_tcp(
        self: &Arc<Self>,
        value: &serde_json::Value,
    ) -> Result<String, String> {
        let _access = self.gateway.settings_lock.lock().await;
        if !self.gateway.is_enabled() {
            return Err("Go online to open a TCP forward".into());
        }
        let host = peer(value)?;
        let service = argument(value, "service")?.to_owned();
        let incarnation = argument(value, "incarnation")?.to_owned();
        self.tcp_permitted(host, &service, &incarnation).await?;
        let mut routes = self.tcp.routes.lock().await;
        let key = (host, service.clone());
        if let Some(route) = routes.get(&key)
            && route.incarnation == incarnation
            && !route.task.is_finished()
        {
            return Ok(format!("tcp://127.0.0.1:{}", route.port));
        }
        routes.remove(&key);
        routes.retain(|_, r| !r.task.is_finished());
        if routes.len() >= 32 {
            return Err("Too many TCP forwards; go offline to close unused forwards".into());
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let weak = Arc::downgrade(self);
        let selected = incarnation.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let slots = Arc::new(Semaphore::new(32));
            let mut check = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = check.tick() => {
                        let Some(client) = weak.upgrade() else { break; };
                        let permitted = tokio::time::timeout(
                            Duration::from_secs(5),
                            client.tcp_permitted(host, &service, &selected),
                        ).await;
                        if !matches!(permitted, Ok(Ok(()))) { break; }
                    }
                    accepted = listener.accept() => {
                        let Ok((mut local, _)) = accepted else { break; };
                        let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                        let Some(client) = weak.upgrade() else { break; };
                        let service = service.clone();
                        let incarnation = selected.clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            let setup = async {
                                client.tcp_permitted(host, &service, &incarnation).await?;
                                if host == client.endpoint.id() {
                                    let services = client.local_authority.as_ref()
                                        .ok_or("Local host unavailable")?
                                        .services().await.map_err(|e| e.to_string())?;
                                    let binding = services.iter().find(|s| {
                                        s.id == service && s.incarnation == incarnation
                                            && s.kind == ServiceKind::Tcp
                                    }).ok_or("Service changed")?.backend;
                                    let stream = TcpStream::connect(binding).await.map_err(|e| e.to_string())?;
                                    Ok::<_, String>((None, Box::new(stream) as Box<dyn crate::gateway::ServiceIo>))
                                } else {
                                    let lease = client.peer_connection(host, Protocol::Http).await?;
                                    let stream = wispnet_agent::tunnel::open_service_bound(
                                        &lease, &service, &incarnation,
                                    ).await.map_err(|e| e.to_string())?;
                                    Ok((Some(lease), Box::new(stream) as Box<dyn crate::gateway::ServiceIo>))
                                }
                            };
                            // Keep the pooled connection lease alive for the entire stream.
                            let opened = tokio::time::timeout(Duration::from_secs(10), setup).await;
                            if let Ok(Ok((_lease, mut remote))) = opened {
                                let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
            // Dropping JoinSet cancels every active stream on revoke/unpair/stop.
        });
        routes.insert(
            key,
            Route {
                incarnation,
                port,
                task,
            },
        );
        Ok(format!("tcp://127.0.0.1:{port}"))
    }
}

impl Client {
    /// Owner-local resolution may create TCP listeners; website discovery stays HTTP-only.
    pub(crate) async fn resolve_connection(
        self: &Arc<Self>,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let host = peer(value)?;
        let id = argument(value, "service")?;
        let selected = if host == self.endpoint.id() {
            let services = self
                .local_authority
                .as_ref()
                .ok_or("Local host unavailable")?
                .services()
                .await
                .map_err(|e| e.to_string())?;
            services
                .into_iter()
                .find(|s| s.id == id && s.kind == ServiceKind::Tcp)
                .map(|s| s.incarnation)
        } else {
            if self.saved_host(host).await?.is_none() {
                return Err("Host not paired".into());
            }
            let catalog = self.connection_catalog(host).await?;
            catalog["services"]
                .as_array()
                .and_then(|s| s.iter().find(|s| s["id"] == id && s["kind"] == "tcp"))
                .and_then(|s| s["incarnation"].as_str())
                .map(str::to_owned)
        };
        if let Some(incarnation) = selected {
            let url = self
                .forward_tcp(
                    &serde_json::json!({"host":host,"service":id,"incarnation":incarnation}),
                )
                .await?;
            let address = url.trim_start_matches("tcp://");
            return Ok(
                serde_json::json!({"kind":"tcp","url":url,"address":address,"host_id":host,"service_id":id,"incarnation":incarnation,"link":format!("wispnet://{host}/{id}")}),
            );
        }
        self.resolve_service(value).await
    }
}
