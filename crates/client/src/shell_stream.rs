//! Raw, owner-local access to the native Shell protocol. This keeps optional
//! frontends independent of the node's identity and Iroh connection internals.
use crate::{
    Client,
    shell_exec::{Cancellation, Registration, Resources},
};
use std::{sync::Arc, time::Duration};
use tokio::{io::DuplexStream, sync::watch};
use wispnet_core::identity::EndpointId;

impl Client {
    /// Open the current Shell service on a pinned host. The returned stream is
    /// binary and carries the native Shell protocol, not a terminal byte pipe.
    /// Local unpair/stop and remote grant/incarnation checks remain authoritative.
    pub async fn open_shell_stream(
        self: &Arc<Self>,
        host: EndpointId,
    ) -> Result<DuplexStream, String> {
        let permit = self.shells.reserve_slot().await?;
        tokio::time::timeout(Duration::from_secs(5), self.exec_host(&host.to_string()))
            .await
            .map_err(|_| "Device lookup timed out")??;
        let (cancel, mut cancelled) = watch::channel(false);
        let id = crate::token();
        let mut executions = self.shells.execs.lock().await;
        if host != self.endpoint.id() && self.saved_host(host).await?.is_none() {
            return Err("Pairing removed while opening Shell stream".into());
        }
        executions.insert(id.clone(), Cancellation { host, cancel });
        drop(executions);
        let registration = Registration {
            id,
            registry: self.shells.execs.clone(),
        };
        let mut resources = Resources::default();
        let mut remote = tokio::time::timeout(
            Duration::from_secs(10),
            self.open_exec_stream(host, &mut resources),
        )
        .await
        .map_err(|_| "Shell stream setup timed out")??;
        // Bounded backpressure in both directions; no output accumulation or
        // UTF-8 conversion. Dropping the local socket drops this connection.
        let (stream, mut worker) = tokio::io::duplex(64 * 1024);
        let client = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _registration = registration;
            let _resources = resources;
            let (mut local_read, mut local_write) = tokio::io::split(&mut worker);
            let (mut remote_read, mut remote_write) = tokio::io::split(&mut remote);
            let pairing = async {
                if host == client.endpoint.id() {
                    std::future::pending::<()>().await;
                }
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if !client
                        .saved_host(host)
                        .await
                        .is_ok_and(|host| host.is_some())
                    {
                        break;
                    }
                }
            };
            tokio::select! {
                biased;
                _ = cancelled.changed() => {},
                _ = pairing => {},
                // Native stdin EOF is an explicit frame. Transport EOF means
                // cancellation; do not leave a blocked remote stdin alive by
                // waiting for both halves of a disconnected socket to finish.
                _ = tokio::io::copy(&mut local_read, &mut remote_write) => {},
                _ = tokio::io::copy(&mut remote_read, &mut local_write) => {},
            }
        });
        Ok(stream)
    }
}
