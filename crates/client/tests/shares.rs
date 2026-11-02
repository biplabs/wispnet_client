#![cfg(unix)]
use serde_json::json;
use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use wispnet_client::{
    Client,
    files::{FileCancellation, FileTarget, TransferOptions},
};
use wispnet_core::{config::TransportConfig, identity::SecretKey};
use wispnet_registry::host::{self, Authority, Mutation};

fn transport() -> TransportConfig {
    serde_json::from_value(json!({"bind_addr":"127.0.0.1:0"})).unwrap()
}
async fn configure(
    state: &Path,
    root: &Path,
    own: &SecretKey,
    peer: Option<&SecretKey>,
) -> FileTarget {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700)).unwrap();
    host::initialize(state, own.public()).unwrap();
    let authority = Authority::open(state, own.public()).unwrap();
    let snapshot = authority.snapshot().await.unwrap();
    let (mut store, owner) = host::owner_store(state, own.public()).unwrap();
    let mut revision = snapshot.revision;
    if let Some(peer) = peer
        && snapshot.clients.is_empty()
    {
        revision = store
            .host_mutate(
                own.public(),
                &owner,
                Mutation::Approve {
                    expected_revision: revision,
                    peer: peer.public(),
                    name: "laptop".into(),
                },
            )
            .unwrap();
    }
    if !snapshot.file_shares.contains_key("checkpoints") {
        store
            .host_mutate(
                own.public(),
                &owner,
                Mutation::FileShare {
                    expected_revision: revision,
                    id: "checkpoints".into(),
                    root: root.to_owned(),
                    allow: peer.map(|p| vec![p.public()]).unwrap_or_default(),
                },
            )
            .unwrap();
    }
    let service = authority
        .services()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.id == "checkpoints")
        .unwrap();
    FileTarget {
        host: own.public(),
        service: service.id,
        incarnation: service.incarnation,
    }
}
fn source(path: &Path) {
    let mut f = std::fs::File::create(path).unwrap();
    for i in 0..3 {
        f.write_all(&vec![i; wispnet_files::MAX_CHUNK]).unwrap();
    }
    f.write_all(b"checkpoint tail").unwrap();
    f.sync_all().unwrap();
}
fn pause() -> TransferOptions {
    let cancellation = FileCancellation::default();
    let stop = cancellation.clone();
    TransferOptions {
        cancellation,
        progress: Some(Arc::new(move |p| {
            if p.phase == "transferring" && p.verified_bytes >= wispnet_files::MAX_CHUNK as u64 {
                stop.cancel();
            }
        })),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_share_browse_download_resume_and_inbox_permissions_are_independent() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let host_state = tempfile::tempdir().unwrap();
        let client_state = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(client_state.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let shared = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let hk = SecretKey::generate();
        let ck = SecretKey::generate();
        let path = shared.path().join("epoch-10.bin");
        source(&path);
        std::fs::create_dir(shared.path().join("subdir")).unwrap();
        std::fs::write(shared.path().join("subdir/config.json"), b"{}").unwrap();
        let target = configure(host_state.path(), shared.path(), &hk, Some(&ck)).await;
        let authority = Authority::open(host_state.path(), hk.public()).unwrap();
        let host =
            wispnet_agent::tunnel::TunnelHost::bind_host(&hk, &transport(), authority.clone())
                .await
                .unwrap();
        wispnet_agent::peers::save(
            client_state.path(),
            &wispnet_core::pairing::Bootstrap {
                version: 1,
                host: hk.public(),
                relay_url: "https://fixture.invalid".into(),
                direct_hint: Some(host.endpoint().bound_sockets()[0]),
            },
        )
        .unwrap();
        let client = Client::new(ck.clone(), client_state.path().to_owned(), transport())
            .await
            .unwrap();
        let page = client
            .list_shared_files(target.clone(), "", None)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 2);
        assert!(page.entries.iter().any(|e| e.name == "subdir" && e.is_dir));
        assert_eq!(
            client
                .list_shared_files(target.clone(), "subdir", None)
                .await
                .unwrap()
                .entries[0]
                .path,
            "subdir/config.json"
        );
        assert_eq!(
            client
                .list_shared_files(target.clone(), "../", None)
                .await
                .unwrap_err()
                .code,
            "PATH_DENIED"
        );
        let output = dest.path().join("epoch.bin");
        assert_eq!(
            client
                .get_file(target.clone(), "epoch-10.bin", &output, pause())
                .await
                .unwrap_err()
                .code,
            "CANCELLED"
        );
        assert!(!output.exists());
        client.endpoint.close().await;
        drop(client);
        let client = Client::new(ck, client_state.path().to_owned(), transport())
            .await
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let progress = seen.clone();
        let info = client
            .get_file(
                target.clone(),
                "epoch-10.bin",
                &output,
                TransferOptions {
                    progress: Some(Arc::new(move |p| {
                        if p.phase == "transferring" {
                            progress.lock().unwrap().push(p.verified_bytes);
                        }
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap()[0], wispnet_files::MAX_CHUNK as u64);
        assert_eq!(wispnet_files::hash_file(&output).unwrap(), info.sha256);
        client
            .get_file(
                target.clone(),
                "epoch-10.bin",
                &output,
                TransferOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            client
                .get_file(
                    target.clone(),
                    "epoch-10.bin",
                    &output,
                    TransferOptions {
                        resume: false,
                        ..Default::default()
                    }
                )
                .await
                .unwrap_err()
                .code,
            "CONFLICT"
        );
        // The application may consume/delete a completed staging artifact;
        // its next request must download again rather than trust a stale offset.
        std::fs::remove_file(&output).unwrap();
        client
            .get_file(
                target.clone(),
                "epoch-10.bin",
                &output,
                TransferOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(wispnet_files::hash_file(&output).unwrap(), info.sha256);
        let snapshot = authority.snapshot().await.unwrap();
        let inbox = snapshot.services.iter().find(|s| s.id == "files").unwrap();
        assert!(inbox.allow.is_empty());
        let inbox_target = FileTarget {
            host: hk.public(),
            service: "files".into(),
            incarnation: inbox.incarnation.clone(),
        };
        assert_eq!(
            client
                .put_file(
                    inbox_target,
                    "",
                    &output,
                    "unsolicited.bin",
                    TransferOptions::default()
                )
                .await
                .unwrap_err()
                .code,
            "PERMISSION_DENIED"
        );
        let (mut store, owner) = host::owner_store(host_state.path(), hk.public()).unwrap();
        store
            .host_mutate(
                hk.public(),
                &owner,
                Mutation::RevokeService {
                    expected_revision: snapshot.revision,
                    service: "checkpoints".into(),
                    peer: client.endpoint.id(),
                },
            )
            .unwrap();
        client
            .command(&json!({"op":"catalog","host":hk.public()}))
            .await
            .unwrap();
        assert_eq!(
            client
                .list_shared_files(target, "", None)
                .await
                .unwrap_err()
                .code,
            "PERMISSION_DENIED"
        );
        client.endpoint.close().await;
        host.endpoint().close().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_resume_rejects_source_changes_and_never_follows_destinations() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let state = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let key = SecretKey::generate();
        let input = shared.path().join("checkpoint.bin");
        source(&input);
        let target = configure(state.path(), shared.path(), &key, None).await;
        let client = Client::new_peer(key, state.path().to_owned(), transport())
            .await
            .unwrap();
        let output = dest.path().join("checkpoint.bin");
        assert_eq!(
            client
                .get_file(target.clone(), "checkpoint.bin", &output, pause())
                .await
                .unwrap_err()
                .code,
            "CANCELLED"
        );
        std::fs::write(&input, b"replacement checkpoint").unwrap();
        assert_eq!(
            client
                .get_file(
                    target.clone(),
                    "checkpoint.bin",
                    &output,
                    TransferOptions::default()
                )
                .await
                .unwrap_err()
                .code,
            "STALE_VERSION"
        );
        client
            .get_file(
                target.clone(),
                "checkpoint.bin",
                &output,
                TransferOptions {
                    resume: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"replacement checkpoint");
        let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let change = changed.clone();
        let changing = input.clone();
        let error = client
            .get_file(
                target.clone(),
                "checkpoint.bin",
                &dest.path().join("changing.bin"),
                TransferOptions {
                    progress: Some(Arc::new(move |p| {
                        if p.phase == "transferring"
                            && p.verified_bytes > 0
                            && !change.swap(true, std::sync::atomic::Ordering::SeqCst)
                        {
                            std::fs::write(&changing, b"changed during transfer").unwrap();
                        }
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "STALE_VERSION");
        assert!(!dest.path().join("changing.bin").exists());
        let victim = dest.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let link = dest.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(
            client
                .get_file(
                    target.clone(),
                    "checkpoint.bin",
                    &link,
                    TransferOptions::default()
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        std::os::unix::fs::symlink(&victim, shared.path().join("escape")).unwrap();
        assert_eq!(
            client
                .get_file(
                    target,
                    "escape",
                    &dest.path().join("escape"),
                    TransferOptions::default()
                )
                .await
                .unwrap_err()
                .code,
            "PATH_DENIED"
        );
        client.endpoint.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folder_download_preserves_tree_and_resumes_across_client_restart() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let state = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let key = SecretKey::generate();
        let run = shared.path().join("run-1");
        std::fs::create_dir_all(run.join("nested/empty")).unwrap();
        source(&run.join("a-checkpoint.bin"));
        std::fs::write(run.join("nested/模型.json"), b"{\"epoch\":10}").unwrap();
        for i in 0..40 {
            std::fs::write(
                run.join(format!("metadata-{i:02}.txt")),
                format!("record {i}"),
            )
            .unwrap();
        }
        let target = configure(state.path(), shared.path(), &key, None).await;
        let client = Client::new_peer(key.clone(), state.path().to_owned(), transport())
            .await
            .unwrap();
        let output = dest.path().join("downloaded-run");
        assert_eq!(
            client
                .get_folder(target.clone(), "run-1", &output, pause())
                .await
                .unwrap_err()
                .code,
            "CANCELLED"
        );
        assert!(
            !output.exists(),
            "partial folder must stay private until verified"
        );
        client.endpoint.close().await;
        drop(client);
        let client = Client::new_peer(key, state.path().to_owned(), transport())
            .await
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let folder = client
            .get_folder(
                target.clone(),
                "run-1",
                &output,
                TransferOptions {
                    progress: Some(Arc::new(move |p| {
                        if p.phase == "transferring" {
                            recorded.lock().unwrap().push(p.verified_bytes);
                        }
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(folder.files_total, 42);
        assert_eq!(seen.lock().unwrap()[0], wispnet_files::MAX_CHUNK as u64);
        assert_eq!(
            std::fs::read(output.join("a-checkpoint.bin")).unwrap(),
            std::fs::read(run.join("a-checkpoint.bin")).unwrap()
        );
        assert!(output.join("nested/empty").is_dir());
        assert_eq!(
            std::fs::read(output.join("nested/模型.json")).unwrap(),
            b"{\"epoch\":10}"
        );
        assert_eq!(
            std::fs::read(output.join("metadata-39.txt")).unwrap(),
            b"record 39"
        );
        // A retained receipt recognizes our completed directory and never
        // substitutes an unrelated pre-existing destination.
        client
            .get_folder(target.clone(), "run-1", &output, TransferOptions::default())
            .await
            .unwrap();
        assert_eq!(
            client
                .get_folder(
                    target.clone(),
                    "run-1",
                    &output,
                    TransferOptions {
                        resume: false,
                        ..Default::default()
                    }
                )
                .await
                .unwrap_err()
                .code,
            "CONFLICT"
        );
        let root_output = dest.path().join("whole-share");
        client
            .get_folder(target, "", &root_output, TransferOptions::default())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root_output.join("run-1/metadata-00.txt")).unwrap(),
            b"record 0"
        );
        client.endpoint.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folder_resume_rejects_changed_membership_and_versions_without_overwriting() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let state = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let key = SecretKey::generate();
        source(&shared.path().join("a-checkpoint.bin"));
        std::fs::write(shared.path().join("z-config.json"), b"original").unwrap();
        let target = configure(state.path(), shared.path(), &key, None).await;
        let client = Client::new_peer(key, state.path().to_owned(), transport())
            .await
            .unwrap();
        let output = dest.path().join("run");
        assert_eq!(
            client
                .get_folder(target.clone(), "", &output, pause())
                .await
                .unwrap_err()
                .code,
            "CANCELLED"
        );
        std::fs::write(shared.path().join("new.txt"), b"new member").unwrap();
        assert_eq!(
            client
                .get_folder(target.clone(), "", &output, TransferOptions::default())
                .await
                .unwrap_err()
                .code,
            "STALE_VERSION"
        );
        assert!(!output.exists());
        client
            .get_folder(
                target.clone(),
                "",
                &output,
                TransferOptions {
                    resume: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(output.join("new.txt")).unwrap(),
            b"new member"
        );
        let changed = shared.path().join("z-config.json");
        let once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let change_once = once.clone();
        let failed = dest.path().join("changing");
        let error = client
            .get_folder(
                target.clone(),
                "",
                &failed,
                TransferOptions {
                    progress: Some(Arc::new(move |p| {
                        if p.phase == "transferring"
                            && p.verified_bytes > 0
                            && !change_once.swap(true, std::sync::atomic::Ordering::SeqCst)
                        {
                            std::fs::write(&changed, b"modified").unwrap();
                        }
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "STALE_VERSION");
        assert!(!failed.exists());
        assert_eq!(
            std::fs::read(output.join("z-config.json")).unwrap(),
            b"original"
        );
        let victim = dest.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("keep"), b"keep").unwrap();
        let symlink = dest.path().join("link");
        std::os::unix::fs::symlink(&victim, &symlink).unwrap();
        assert_eq!(
            client
                .get_folder(target, "", &symlink, TransferOptions::default())
                .await
                .unwrap_err()
                .code,
            "CONFLICT"
        );
        assert_eq!(std::fs::read(victim.join("keep")).unwrap(), b"keep");
        client.endpoint.close().await;
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_stat_and_expected_version_pin_download_before_publication() {
    let state = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let key = SecretKey::generate();
    std::fs::write(shared.path().join("config.json"), b"original").unwrap();
    let target = configure(state.path(), shared.path(), &key, None).await;
    let client = Client::new_peer(key, state.path().to_owned(), transport())
        .await
        .unwrap();
    let inspected = client.command(&json!({"op":"file_stat","host":target.host,"service":target.service,"incarnation":target.incarnation,"path":"config.json"})).await.unwrap();
    let info = loop {
        let status = client
            .command(&json!({"op":"file_status","transfer":inspected["transfer"]}))
            .await
            .unwrap();
        if status["state"] == "running" {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        assert_eq!(status["state"], "complete");
        break status["file"].clone();
    };
    assert_eq!(info["size_bytes"], 8);
    std::fs::write(shared.path().join("config.json"), b"changed").unwrap();
    let started = client.command(&json!({"op":"file_get","host":target.host,"service":target.service,"incarnation":target.incarnation,"path":"config.json","expected_version":info["file_version"],"local_path":dest.path().join("config.json")})).await.unwrap();
    let handle = started["transfer"].clone();
    loop {
        let status = client
            .command(&json!({"op":"file_status","transfer":handle}))
            .await
            .unwrap();
        if status["state"] == "running" {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        assert_eq!(status["state"], "failed");
        assert_eq!(status["error"]["code"], "STALE_VERSION");
        break;
    }
    assert!(!dest.path().join("config.json").exists());
    client.endpoint.close().await;
}
