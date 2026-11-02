//! Real Iroh fault injection and durable baseline checks; safe on a headless host.
use iroh::{Endpoint, endpoint::presets};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    sync::Mutex,
    task::{JoinHandle, JoinSet},
};
use wispnet_client::Client;
use wispnet_core::{host, identity::SecretKey, pairing::Bootstrap};

#[derive(Clone)]
struct Reply {
    bytes: Vec<u8>,
    advertised: Option<u32>,
    trailing: bool,
}
struct Fixture {
    endpoint: Endpoint,
    reply: Arc<Mutex<Reply>>,
    worker: JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::generate())
            .alpns(vec![host::ALPN.to_vec()])
            .bind_addr("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap())
            .unwrap()
            .bind()
            .await
            .unwrap();
        let reply = Arc::new(Mutex::new(Reply {
            bytes: vec![],
            advertised: None,
            trailing: false,
        }));
        let server = endpoint.clone();
        let responses = reply.clone();
        let worker = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
                    incoming=server.accept()=>{
                        let Some(incoming)=incoming else {break};let responses=responses.clone();
                        tasks.spawn(async move {
                            let Ok(connection)=incoming.await else{return};
                            while let Ok((mut send,mut recv))=connection.accept_bi().await {
                                if recv.read_to_end(4096).await.is_err(){continue;}
                                let reply=responses.lock().await.clone();
                                let size=reply.advertised.unwrap_or(reply.bytes.len() as u32);
                                if send.write_all(&size.to_be_bytes()).await.is_err(){continue;}
                                if send.write_all(&reply.bytes).await.is_err(){continue;}
                                if reply.trailing {let _=send.write_all(b"x").await;}
                                let _=send.finish();let _=send.stopped().await;
                            }
                        });
                    }
                }
            }
            tasks.abort_all();
        });
        Self {
            endpoint,
            reply,
            worker,
        }
    }
    fn snapshot(&self, revision: u64) -> Value {
        json!({"version":1,"complete":true,"service_count":1,
            "host":self.endpoint.id(),"generation":"c".repeat(64),"revision":revision,
            "services":[{"id":"website","incarnation":"a".repeat(64),"display_name":"Website","kind":"http"}]})
    }
    async fn set(&self, value: &Value) {
        *self.reply.lock().await = Reply {
            bytes: serde_json::to_vec(value).unwrap(),
            advertised: None,
            trailing: false,
        };
    }
    async fn pin(&self, state: &std::path::Path) {
        wispnet_agent::peers::save(
            state,
            &Bootstrap {
                version: 1,
                host: self.endpoint.id(),
                relay_url: "https://fixture.invalid".into(),
                direct_hint: Some(self.endpoint.bound_sockets()[0]),
            },
        )
        .unwrap();
    }
    async fn close(self) {
        self.worker.abort();
        let _ = self.worker.await;
        self.endpoint.close().await;
    }
}

#[tokio::test]
async fn partial_stale_malformed_and_clock_changes_cannot_replace_a_complete_snapshot() {
    let state = tempfile::tempdir().unwrap();
    fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let faulty = Fixture::new().await;
    let healthy = Fixture::new().await;
    faulty.pin(state.path()).await;
    healthy.pin(state.path()).await;
    let key = SecretKey::generate();
    let config = serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
    let client = Client::new(key.clone(), state.path().into(), config)
        .await
        .unwrap();
    let good = faulty.snapshot(10);
    faulty.set(&good).await;
    healthy.set(&healthy.snapshot(1)).await;
    assert_eq!(client.catalog(faulty.endpoint.id()).await.unwrap(), good);
    let baseline = state
        .path()
        .join("catalog-baselines")
        .join(format!("{}.json", faulty.endpoint.id()));
    let saved = fs::read(&baseline).unwrap();
    let mut bad = Vec::new();
    for (field, value) in [
        ("complete", json!(false)),
        ("version", json!(2)),
        ("service_count", json!(2)),
        ("revision", json!(9)),
        ("revision", json!(-1)),
        ("revision", json!(true)),
        ("generation", json!("d".repeat(64))),
        ("host", json!(healthy.endpoint.id())),
        ("revision", json!(u64::MAX)),
        ("unknown", json!("extra field")),
    ] {
        let mut c = good.clone();
        c[field] = value;
        bad.push(c);
    }
    let mut missing = good.clone();
    missing.as_object_mut().unwrap().remove("complete");
    bad.push(missing);
    let mut inconsistent = good.clone();
    inconsistent["services"][0]["display_name"] = json!("Changed");
    bad.push(inconsistent);
    let mut duplicate = good.clone();
    duplicate["services"]
        .as_array_mut()
        .unwrap()
        .push(good["services"][0].clone());
    duplicate["service_count"] = json!(2);
    bad.push(duplicate);
    let mut unsorted = good.clone();
    let mut service = good["services"][0].clone();
    service["id"] = json!("aaa");
    unsorted["services"].as_array_mut().unwrap().push(service);
    unsorted["service_count"] = json!(2);
    bad.push(unsorted);
    for (field, value) in [
        ("kind", json!("shell")),
        ("display_name", json!("bad\nname")),
        ("display_name", json!("x".repeat(129))),
        ("id", json!("../website")),
        ("incarnation", json!("short")),
        ("display_name", json!(123)),
    ] {
        let mut c = good.clone();
        c["services"][0][field] = value;
        bad.push(c);
    }
    for catalog in bad {
        faulty.set(&catalog).await;
        assert!(
            client.catalog(faulty.endpoint.id()).await.is_err(),
            "accepted {catalog}"
        );
        assert_eq!(fs::read(&baseline).unwrap(), saved);
        assert!(
            client.catalog(healthy.endpoint.id()).await.is_ok(),
            "bad host blocked independent host"
        );
    }
    for reply in [
        Reply {
            bytes: String::from_utf8(serde_json::to_vec(&good).unwrap())
                .unwrap()
                .replace("\"revision\":10", "\"revision\":10,\"revision\":11")
                .into_bytes(),
            advertised: None,
            trailing: false,
        },
        Reply {
            bytes: b"{".to_vec(),
            advertised: None,
            trailing: false,
        },
        Reply {
            bytes: serde_json::to_vec(&good).unwrap(),
            advertised: Some(100_000),
            trailing: false,
        },
        Reply {
            bytes: serde_json::to_vec(&good).unwrap(),
            advertised: None,
            trailing: true,
        },
        Reply {
            bytes: vec![],
            advertised: Some(host::MAX_RESPONSE as u32 + 1),
            trailing: false,
        },
    ] {
        *faulty.reply.lock().await = reply;
        assert!(client.catalog(faulty.endpoint.id()).await.is_err());
        assert_eq!(fs::read(&baseline).unwrap(), saved);
    }
    // A damaged private baseline cannot silently become a fresh trust baseline.
    fs::write(&baseline, b"{").unwrap();
    faulty.set(&good).await;
    assert!(client.catalog(faulty.endpoint.id()).await.is_err());
    assert!(client.catalog(healthy.endpoint.id()).await.is_ok());
    fs::write(&baseline, &saved).unwrap();
    // Display cache deletion and backwards/forwards filesystem times never reset high-water.
    let display = state.path().join("desktop-catalogs");
    fs::create_dir(&display).unwrap();
    fs::remove_dir(&display).unwrap();
    for modified in [
        SystemTime::UNIX_EPOCH,
        SystemTime::now() + Duration::from_secs(31_536_000),
    ] {
        fs::File::options()
            .write(true)
            .open(&baseline)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        faulty.set(&faulty.snapshot(9)).await;
        assert!(client.catalog(faulty.endpoint.id()).await.is_err());
    }
    client.endpoint.close().await;
    drop(client);
    let config = serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap();
    let restarted = Client::new(key, state.path().into(), config).await.unwrap();
    assert!(restarted.catalog(faulty.endpoint.id()).await.is_err());
    // A newer complete empty snapshot removes the peer's whole filtered listing atomically.
    let mut removed = faulty.snapshot(11);
    removed["services"] = json!([]);
    removed["service_count"] = json!(0);
    faulty.set(&removed).await;
    assert_eq!(
        restarted.catalog(faulty.endpoint.id()).await.unwrap(),
        removed
    );
    faulty.set(&good).await;
    assert!(restarted.catalog(faulty.endpoint.id()).await.is_err());
    let epoch = wispnet_agent::catalogs::begin(state.path(), faulty.endpoint.id()).unwrap();
    wispnet_agent::catalogs::reset(state.path(), faulty.endpoint.id()).unwrap();
    let old: host::Catalog = serde_json::from_value(good).unwrap();
    assert!(
        wispnet_agent::catalogs::accept(state.path(), faulty.endpoint.id(), &epoch, &old).is_err()
    );
    let mut recovered = faulty.snapshot(1);
    recovered["generation"] = json!("d".repeat(64));
    faulty.set(&recovered).await;
    assert_eq!(
        restarted.catalog(faulty.endpoint.id()).await.unwrap(),
        recovered
    );
    restarted.endpoint.close().await;
    faulty.close().await;
    healthy.close().await;
}
