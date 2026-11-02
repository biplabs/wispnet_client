#!/usr/bin/env python3
"""Real CLI Files checks over two isolated native Iroh/QUIC nodes.

Temporary identities, policies, homes, exports and loopback pairing hints keep
this test independent of installed nodes and the user's existing grants.
"""

import hashlib
import json
import os
import subprocess
import time
import unittest

import shell_smoke


class FilesSmoke(unittest.TestCase):
    # Reuse the isolated node fixture without inheriting its Shell test cases.
    setUp = shell_smoke.ShellSmoke.setUp
    tearDown = shell_smoke.ShellSmoke.tearDown
    host = shell_smoke.ShellSmoke.host
    cli = shell_smoke.ShellSmoke.cli
    mutate = shell_smoke.ShellSmoke.mutate

    def test_named_inboxes_keep_roots_and_device_permissions_independent(self):
        client, api, client_id, _ = self.host("inbox-sender")
        target, target_api, target_id, target_port = self.host("inbox-receiver")
        first, second = self.root.resolve() / "first-inbox", self.root.resolve() / "second-inbox"
        first.mkdir(mode=0o700)
        second.mkdir(mode=0o700)
        invitation_path = self.root / "inbox-invitation.json"
        subprocess.run([shell_smoke.BINARY, "host-invite", "--config", target,
                        "--output", invitation_path], capture_output=True, check=True,
                       env=self.environment, timeout=15)
        invitation = json.loads(invitation_path.read_text())
        invitation["direct_hint"] = f"127.0.0.1:{target_port}"
        api.request(op="pair", host=target_id, invitation=json.dumps(invitation))
        self.mutate(target_api, op="approve_invitation", invitation=invitation["invitation"])
        self.cli(target, "inbox", "first-inbox", "--path", first, "--allow", client_id)
        self.cli(target, "inbox", "second-inbox", "--path", second, "--allow", "none")
        catalog = api.request(op="catalog", host=target_id)
        self.assertEqual([(s["id"], s["kind"]) for s in catalog["services"]],
                         [("first-inbox", "file_inbox")])
        self.assertNotIn(str(first), json.dumps(catalog))
        source = self.root / "named-source.bin"
        content = os.urandom(1024 * 1024 + 23)
        source.write_bytes(content)
        uploaded = self.cli(client, "files", "put", target_id, "first-inbox", source, "same-name.bin")
        self.completed(uploaded, hashlib.sha256(content).hexdigest(), len(content))
        self.assertEqual((first / "same-name.bin").read_bytes(), content)
        self.assertFalse((second / "same-name.bin").exists())
        denied = self.cli(client, "files", "put", target_id, "second-inbox", source, "same-name.bin", success=False)
        self.assertEqual(json.loads(denied.stdout)["error"]["code"], "PERMISSION_DENIED")
        target_service = catalog["services"][0]
        job = api.request(op="file_put", host=target_id, service="first-inbox",
                          incarnation=target_service["incarnation"], workspace="second-inbox",
                          local_path=str(source), remote_path="cross-inbox.bin", resume=True)
        import time
        deadline = time.monotonic() + 15
        while job["state"] == "running" and time.monotonic() < deadline:
            time.sleep(.05)
            job = api.request(op="file_status", transfer=job["transfer"])
        self.assertEqual(job["state"], "failed", job)
        self.assertFalse((first / "cross-inbox.bin").exists())
        self.assertFalse((second / "cross-inbox.bin").exists())
        self.cli(target, "inbox", "second-inbox", "--allow", client_id)
        self.cli(client, "files", "put", target_id, "second-inbox", source, "same-name.bin")
        self.assertEqual((second / "same-name.bin").read_bytes(), content)
        self.cli(target, "inbox", "first-inbox", "--allow", "none")
        self.cli(client, "files", "put", target_id, "first-inbox", source, "revoked.bin", success=False)
        self.cli(client, "files", "put", target_id, "second-inbox", source, "still-allowed.bin")
        self.assertFalse((first / "revoked.bin").exists())
        self.assertEqual((second / "still-allowed.bin").read_bytes(), content)
        self.cli(client, "files", "get", target_id, "second-inbox", "same-name.bin",
                 self.root / "unauthorized-read.bin", success=False)
        self.assertFalse((self.root / "unauthorized-read.bin").exists())
        self.cli(target, "inbox", "first-inbox", "--remove")
        self.assertEqual((first / "same-name.bin").read_bytes(), content)

    def test_read_only_shares_have_independent_grants_and_cli_browse_download(self):
        client, api, client_id, _ = self.host("share-reader")
        target, target_api, target_id, target_port = self.host("share-provider")
        source = self.root.resolve() / "checkpoints"
        source.mkdir(mode=0o700)
        (source / "run-1").mkdir()
        content = os.urandom(5 * 1024 * 1024 + 19)
        (source / "run-1" / "model.bin").write_bytes(content)
        invitation_path = self.root / "share-invitation.json"
        subprocess.run(
            [shell_smoke.BINARY, "host-invite", "--config", target,
             "--output", invitation_path],
            capture_output=True, check=True, env=self.environment, timeout=15,
        )
        invitation = json.loads(invitation_path.read_text())
        invitation["direct_hint"] = f"127.0.0.1:{target_port}"
        api.request(op="pair", host=target_id, invitation=json.dumps(invitation))
        self.mutate(target_api, op="approve_invitation", invitation=invitation["invitation"])
        inbox_before = json.loads(self.cli(target, "files-access").stdout)
        receiver_before = json.loads(self.cli(client, "files-access").stdout)
        created = json.loads(self.cli(target, "share", "checkpoints", "--path", str(source),
                                      "--allow", client_id).stdout)
        self.assertEqual(created["allow"], [client_id])
        self.cli(target, "share", "private", "--path", str(source), "--allow", "none")
        catalog = api.request(op="catalog", host=target_id)
        self.assertEqual([(s["id"], s["kind"]) for s in catalog["services"]],
                         [("checkpoints", "file_share")])
        self.assertNotIn(str(source), json.dumps(catalog))
        service = catalog['services'][0]
        inspection = api.request(op='file_stat', host=target_id, service='checkpoints',
                                 incarnation=service['incarnation'], path='run-1/model.bin')
        self.assertIn('transfer', inspection)
        deadline = time.monotonic() + 10
        while inspection['state'] == 'running' and time.monotonic() < deadline:
            time.sleep(.02)
            inspection = api.request(op='file_status', transfer=inspection['transfer'])
        self.assertEqual(inspection['state'], 'complete', inspection)
        self.assertEqual(inspection['file']['sha256'], hashlib.sha256(content).hexdigest())
        listed = json.loads(self.cli(client, "files", "ls", target_id, "checkpoints").stdout)
        self.assertEqual([(e["name"], e["is_dir"]) for e in listed["entries"]], [("run-1", True)])
        listed = json.loads(self.cli(client, "files", "ls", target_id, "checkpoints", "run-1").stdout)
        self.assertEqual(listed["entries"][0]["path"], "run-1/model.bin")
        destination = self.root / "chosen-local-name.bin"
        downloaded = self.cli(client, "files", "get", target_id, "checkpoints", "run-1/model.bin", str(destination))
        self.completed(downloaded, hashlib.sha256(content).hexdigest(), len(content))
        self.assertEqual(destination.read_bytes(), content)
        # A folder uses one native background job, including paginated children,
        # empty directories and original Unicode names. It creates no archives
        # in the source share and resumes a completed destination idempotently.
        (source / 'run-1' / 'empty-directory').mkdir()
        (source / 'run-1' / 'nested').mkdir()
        small_files = {f'page-{index:02}.json':
                       (json.dumps({'step': index, 'status': 'checkpoint ready'}) + '\n').encode() * 100
                       for index in range(40)}
        small_files['nested/資料😀.txt'] = 'checkpoint 資料 😀\n'.encode() * 100
        small_files['empty.bin'] = b''
        for path, data in small_files.items():
            (source / 'run-1' / path).write_bytes(data)
        folder = self.root / 'downloaded-checkpoint'
        result = self.cli(client, 'files', 'get', target_id, 'checkpoints', 'run-1',
                          folder, '--recursive')
        complete = json.loads(result.stdout)
        self.assertEqual(complete['state'], 'complete', complete)
        self.assertEqual(complete['files_total'], len(small_files) + 1)
        self.assertEqual(complete['files_complete'], len(small_files) + 1)
        self.assertEqual(complete['verified_bytes'], len(content) + sum(map(len, small_files.values())))
        self.assertEqual((folder / 'model.bin').read_bytes(), content)
        self.assertTrue((folder / 'empty-directory').is_dir())
        for path, data in small_files.items():
            self.assertEqual((folder / path).read_bytes(), data)
        retry = json.loads(self.cli(client, 'files', 'get', target_id, 'checkpoints',
                                   'run-1', folder, '--recursive').stdout)
        self.assertEqual(retry['state'], 'complete', retry)
        self.assertFalse(list(source.rglob('*.zip')))
        self.assertEqual(json.loads(self.cli(target, "files-access").stdout), inbox_before)
        self.assertEqual(json.loads(self.cli(client, "files-access").stdout), receiver_before)
        denied = self.cli(client, "files", "ls", target_id, "private", success=False)
        self.assertNotEqual(denied.returncode, 0)
        uploaded = self.cli(client, "files", "put", target_id, "", str(destination), "unsolicited.bin", success=False)
        self.assertEqual(json.loads(uploaded.stdout)["error"]["code"], "PERMISSION_DENIED")
        self.cli(target, "share", "checkpoints", "--allow", "none")
        self.assertEqual(api.request(op="catalog", host=target_id)["services"], [])
        self.cli(client, "files", "get", target_id, "checkpoints", "run-1/model.bin",
                 str(self.root / "revoked.bin"), success=False)
        self.assertFalse((self.root / "revoked.bin").exists())
        self.cli(target, "share", "checkpoints", "--remove")
        self.assertEqual((source / "run-1" / "model.bin").read_bytes(), content)

    def completed(self, result, expected_hash, size):
        completion = json.loads(result.stdout)
        self.assertEqual(completion["state"], "complete", completion)
        self.assertEqual(completion["file"]["sha256"], expected_hash)
        self.assertEqual(completion["file"]["size_bytes"], size)
        self.assertTrue(completion["file"]["file_version"])
        progress = [json.loads(line) for line in result.stderr.splitlines()]
        self.assertTrue(progress, "Files CLI emitted no machine-readable progress")
        self.assertEqual(progress[-1]["state"], "complete")
        self.assertEqual(progress[-1]["verified_bytes"], size)
        # Progress and completion contain metadata, never the file bytes.
        self.assertLess(len(result.stdout), 8192)
        self.assertTrue(all(len(line) < 8192 for line in result.stderr.splitlines()))
        return completion

    def test_native_cli_transfer_grants_retries_and_path_confinement(self):
        client, api, client_id, _ = self.host("files-client")
        target, target_api, target_id, target_port = self.host("files-target")
        export = self.root.resolve() / "export"
        export.mkdir(mode=0o700)
        source = self.root / "dataset.bin"
        content = os.urandom(5 * 1024 * 1024 + 137)
        source.write_bytes(content)
        expected_hash = hashlib.sha256(content).hexdigest()

        initial = json.loads(self.cli(target, "files-access").stdout)
        self.assertEqual(initial["service"]["id"], "files")
        self.assertEqual(initial["service"]["kind"], "files")
        self.assertEqual(initial["service"]["allow"], [])
        self.assertEqual(initial["exports"], {})
        invitation_path = self.root / "invitation.json"
        subprocess.run(
            [shell_smoke.BINARY, "host-invite", "--config", target,
             "--output", invitation_path],
            capture_output=True, check=True, env=self.environment, timeout=15,
        )
        invitation = json.loads(invitation_path.read_text())
        invitation["direct_hint"] = f"127.0.0.1:{target_port}"
        api.request(op="pair", host=target_id, invitation=json.dumps(invitation))
        self.mutate(target_api, op="approve_invitation",
                    invitation=invitation["invitation"])

        # Pairing and a Shell grant must not grant Files access.
        self.cli(target, "shell-access", "--allow", client_id)
        catalog = api.request(op="catalog", host=target_id)
        self.assertEqual([service["id"] for service in catalog["services"]], ["shell"])
        # A folder must be configured before any UI/CLI/API can grant Files.
        before_folder = target_api.request(op="status")["host"]
        no_folder = self.cli(target, "files-access", "--allow", client_id,
                             success=False)
        self.assertIn("folder", no_folder.stderr.lower())
        self.assertEqual(target_api.request(op="status")["host"], before_folder)
        with self.assertRaises(ValueError):
            self.mutate(target_api, op="grant_service", service="files",
                        peer=client_id)
        self.assertEqual(target_api.request(op="status")["host"], before_folder)
        self.assertEqual(api.request(op="catalog", host=target_id)["services"],
                         catalog["services"])

        denied = self.cli(client, "files", "put", target_id, "training.v1",
                          str(source), "jobs/a/input.bin", success=False)
        self.assertIn("not granted", denied.stderr)
        self.assertEqual(json.loads(denied.stdout)["error"]["code"], "PERMISSION_DENIED")
        self.assertFalse((export / "jobs/a/input.bin").exists())

        configured = json.loads(self.cli(
            target, "files-access", "--export", f"training.v1={export}",
            "--allow", "none",
        ).stdout)
        self.assertEqual(configured["exports"], {"training.v1": str(export)})
        granted = json.loads(self.cli(target, "files-access", "--allow", client_id).stdout)
        self.assertEqual(granted["allow"], [client_id])
        self.assertEqual(granted["exports"], configured["exports"])
        catalog = api.request(op="catalog", host=target_id)
        files_service = next(service for service in catalog["services"]
                             if service["id"] == "files")
        workspaces = api.request(op="file_workspaces", host=target_id,
                                 incarnation=files_service["incarnation"])
        self.assertEqual(workspaces, {"workspaces": ["training.v1"]})
        self.assertNotIn(str(export), json.dumps(workspaces))
        put = self.cli(client, "files", "put", target_id, "training.v1",
                       str(source), "jobs/a/input.bin")
        uploaded = self.completed(put, expected_hash, len(content))
        self.assertEqual(hashlib.sha256((export / "jobs/a/input.bin").read_bytes()).hexdigest(), expected_hash)
        self.assertEqual(uploaded["file"]["path"], "jobs/a/input.bin")

        # A grant is ongoing: retries and further sends need no new confirmation.
        # Repeating a completed helper call recognizes the same destination.
        retry = self.cli(client, "files", "put", target_id, "training.v1",
                         str(source), "jobs/a/input.bin")
        self.completed(retry, expected_hash, len(content))
        additional = self.cli(client, "files", "put", target_id, "training.v1",
                              str(source), "jobs/b/input.bin")
        self.completed(additional, expected_hash, len(content))
        self.assertEqual((export / "jobs/b/input.bin").read_bytes(), content)

        # Receiving permission never lets the sender retrieve the host's files.
        downloaded = self.root / "downloaded.bin"
        get = self.cli(client, "files", "get", target_id, "files",
                       "jobs/a/input.bin", str(downloaded), success=False)
        self.assertFalse(downloaded.exists())
        self.assertIn("share", get.stderr.lower())

        # Identical existing content is not this sender's completed transfer.
        existing = export / "existing.bin"
        existing.write_bytes(content)
        conflict = self.cli(client, "files", "put", target_id, "training.v1",
                            str(source), "existing.bin", success=False)
        self.assertEqual(json.loads(conflict.stdout)["error"]["code"], "CONFLICT")
        self.assertEqual(existing.read_bytes(), content)

        outside = self.root / "outside.bin"
        escaped = self.cli(client, "files", "put", target_id, "training.v1",
                           str(source), "../outside.bin", success=False)
        failure = json.loads(escaped.stdout)
        self.assertEqual(failure["state"], "failed")
        self.assertEqual(failure["error"]["code"], "PATH_DENIED")
        self.assertFalse(outside.exists())

        # Files revocation leaves Shell access and the completed artifact intact.
        self.cli(target, "files-access", "--allow", "none")
        catalog = api.request(op="catalog", host=target_id)
        self.assertEqual([service["id"] for service in catalog["services"]], ["shell"])
        with self.assertRaisesRegex(ValueError, "PERMISSION_DENIED"):
            api.request(op="file_workspaces", host=target_id,
                        incarnation=files_service["incarnation"])
        revoked = self.cli(client, "files", "put", target_id, "training.v1",
                           str(source), "jobs/revoked.bin", success=False)
        self.assertIn("not granted", revoked.stderr)
        self.assertEqual(json.loads(revoked.stdout)["error"]["code"], "PERMISSION_DENIED")
        self.assertFalse((export / "jobs/revoked.bin").exists())
        self.assertEqual(hashlib.sha256((export / "jobs/a/input.bin").read_bytes()).hexdigest(), expected_hash)

        # Removing the last folder revokes standing access without deleting data.
        self.cli(target, "files-access", "--allow", client_id)
        before_clear = target_api.request(op="status")["host"]
        conflicting_clear = self.cli(target, "files-access", "--clear-exports",
                                     "--allow", client_id, success=False)
        self.assertIn("folder", conflicting_clear.stderr.lower())
        self.assertEqual(target_api.request(op="status")["host"], before_clear)
        cleared = json.loads(self.cli(target, "files-access", "--clear-exports").stdout)
        self.assertEqual(cleared["allow"], [])
        self.assertEqual(cleared["exports"], {})
        snapshot = target_api.request(op="status")["host"]
        self.assertEqual(snapshot["file_exports"], {})
        files = next(service for service in snapshot["services"]
                     if service["id"] == "files")
        self.assertEqual(files["allow"], [])
        catalog = api.request(op="catalog", host=target_id)
        self.assertEqual([service["id"] for service in catalog["services"]], ["shell"])
        self.assertEqual(hashlib.sha256((export / "jobs/a/input.bin").read_bytes()).hexdigest(), expected_hash)


if __name__ == "__main__":
    unittest.main()
