package com.biplabs.wispnet;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Context;
import android.content.Intent;
import android.content.UriPermission;
import android.net.Uri;
import android.os.Bundle;
import android.os.SystemClock;
import android.provider.DocumentsContract;
import android.view.accessibility.AccessibilityNodeInfo;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import org.json.JSONArray;
import org.json.JSONObject;

/** Opt-in disposable-emulator proof of read-only SAF sources over native QUIC. */
final class AndroidSharesChecks {
 private final Instrumentation test;
 private final Bundle arguments;
 private NetworkBridge bridge;
 private final JSONArray checks = new JSONArray();
 AndroidSharesChecks(Instrumentation test, Bundle arguments) { this.test = test; this.arguments = new Bundle(arguments); }
 void run() {
  JSONObject report = new JSONObject(); Bundle result = new Bundle();
  try {
   Context context = test.getTargetContext();
   Activity activity = test.startActivitySync(new Intent(context, FlutterMainActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
   context.startForegroundService(new Intent(context, AccessService.class));
   bridge = new NetworkBridge(context);
   request("setAccess", new JSONObject().put("enabled", true));
   String host = arguments.getString("host");
   request("pair", new JSONObject().put("code", arguments.getString("invitation")));
   boolean paired = false;
   for (int attempt = 0; attempt < 100 && !paired; attempt++) {
    request("refreshHost", new JSONObject().put("host", host));
    JSONArray clients = request("sharesStatus", new JSONObject()).getJSONArray("clients");
    for (int index=0; index<clients.length(); index++) if (host.equals(clients.getJSONObject(index).getString("peer"))) paired = true;
    if (!paired) Thread.sleep(100);
   }
   require(paired, "Paired computer missing from share permissions");
   JSONObject inbox = inbox(request("receivingStatus", new JSONObject()));
   String folderName = arguments.getString("shared_folder");
   require(folderName != null && folderName.matches("WispnetShareTest-[0-9a-f]{12}"), "Use an isolated fixture folder");
   Uri tree = DocumentsContract.buildTreeDocumentUri("com.android.externalstorage.documents", "primary:Documents/" + folderName);
   Intent picker = new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE)
       .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION | Intent.FLAG_GRANT_PREFIX_URI_PERMISSION)
       .putExtra(DocumentsContract.EXTRA_INITIAL_URI, DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree)));
   test.runOnMainSync(() -> activity.startActivityForResult(picker, 106));
   click("Use this folder", 20000); click("Allow", 10000); Thread.sleep(500);
   context.getContentResolver().takePersistableUriPermission(tree, Intent.FLAG_GRANT_READ_URI_PERMISSION);
   boolean readOnly = false;
   for (UriPermission permission : context.getContentResolver().getPersistedUriPermissions())
    if (tree.equals(permission.getUri())) readOnly = permission.isReadPermission() && !permission.isWritePermission();
   require(readOnly, "Source tree must persist READ permission only");
   JSONObject folder = request("prepareShareFolder", new JSONObject().put("uri", tree.toString()));
   JSONObject status = request("sharesStatus", new JSONObject());
   JSONObject config = new JSONObject().put("expected_revision", status.getLong("revision"))
       .put("id", "checkpoints").put("folder", folder.getString("folder")).put("allow", new JSONArray().put(host));
   request("saveShare", config);
   boolean stale = false; try { request("saveShare", config); } catch (Exception conflict) { stale = true; }
   require(stale, "Stale share policy revision accepted");
   status = request("sharesStatus", new JSONObject());
   request("saveShare", new JSONObject().put("expected_revision", status.getLong("revision"))
       .put("id", "private").put("folder", folder.getString("folder")).put("allow", new JSONArray()));
   checks.put("system folder picker and persisted READ-only SAF tree");
   checks.put("per-share explicit device permissions and revision rejection");
   AndroidShares source = new AndroidShares(context);
   JSONObject traversal = decode(source.request(new JSONObject().put("root", folder.getString("folder"))
       .put("request", new JSONObject().put("op", "share_list").put("path", "../")), () -> false));
   require("PATH_DENIED".equals(traversal.optString("code")), "Traversal accepted by SAF adapter");
   JSONObject cancelled = decode(source.request(new JSONObject(), () -> true));
   require("UNAVAILABLE".equals(cancelled.optString("code")), "Cancellation ignored by SAF adapter");
   checks.put("SAF traversal rejection and immediate cancellation");
   JSONObject sourceRequest = new JSONObject().put("root", folder.getString("folder"));
   JSONObject pinned = decode(source.request(sourceRequest.put("request",
       new JSONObject().put("op", "share_stat").put("path", "model.bin")), () -> false)).getJSONObject("file");
   for (int index = 0; index < 140; index++) {
    JSONObject metadata = decode(source.request(sourceRequest.put("request", new JSONObject().put("op", "share_stat")
        .put("path", String.format(java.util.Locale.ROOT, "page-%02d.txt", index))), () -> false));
    require("file".equals(metadata.optString("type")), "Source cache fixture failed to pin metadata");
   }
   byte[] refreshed = source.request(sourceRequest.put("request", new JSONObject().put("op", "share_read_range")
       .put("path", "model.bin").put("expected_version", pinned.getString("file_version")).put("offset", 0).put("max_bytes", 64)), () -> false);
   require("data".equals(decode(refreshed).optString("type")), "Evicted manifest file could not rebuild its pinned source version");
   ByteBuffer refreshedBytes = ByteBuffer.wrap(refreshed); int metadataSize = refreshedBytes.getInt(), payloadSize = refreshedBytes.getInt();
   require(payloadSize == 64, "Rebuilt source version returned the wrong range length");
   for (int index = 0; index < payloadSize; index++) require((refreshed[8 + metadataSize + index] & 255) == index, "Rebuilt source version changed bytes");
   JSONObject wrong = decode(source.request(sourceRequest.put("request", new JSONObject().put("op", "share_read_range")
       .put("path", "model.bin").put("expected_version", "0".repeat(64)).put("offset", 0).put("max_bytes", 64)), () -> false));
   require("STALE_VERSION".equals(wrong.optString("code")), "Source cache refresh accepted another file version");
   checks.put("folder manifests exceeding the bounded source version cache refresh exact pinned versions without changing bytes");
   JSONObject snapshot = request("snapshot", new JSONObject()); String peer = snapshot.getString("peer");
   JSONArray local = snapshot.getJSONObject("hosts").getJSONObject(peer).getJSONObject("catalog").getJSONArray("services");
   boolean localShare = false; for (int n=0; n<local.length(); n++) if ("checkpoints".equals(local.getJSONObject(n).optString("id"))) localShare = true;
   require(localShare, "Configured shares missing from local app card");
   test.runOnMainSync(() -> activity.moveTaskToBack(true));
   signal("download", peer); JSONObject downloaded = await("download"); require(downloaded.getBoolean("success"), downloaded.toString());
   checks.put("background native QUIC browse/download, pagination, Unicode and SHA verification");
   JSONObject share = share("checkpoints");
   request("setServiceEnabled", new JSONObject().put("service", "checkpoints").put("incarnation", share.getString("incarnation")).put("enabled", false));
   signal("disabled", peer); require(await("disabled").getBoolean("success"), "Disabled share was accessible");
   request("setServiceEnabled", new JSONObject().put("service", "checkpoints").put("incarnation", share.getString("incarnation")).put("enabled", true));
   signal("enabled", peer); require(await("enabled").getBoolean("success"), "Enabled source share failed");
   status = request("sharesStatus", new JSONObject());
   request("saveShare", new JSONObject().put("expected_revision", status.getLong("revision"))
       .put("id", "checkpoints").put("folder", folder.getString("folder")).put("allow", new JSONArray()));
   signal("revoked", peer); require(await("revoked").getBoolean("success"), "Revoked source remained accessible");
   checks.put("share pause, resume and independent grant revocation");
   status = request("sharesStatus", new JSONObject());
   request("saveShare", new JSONObject().put("expected_revision", status.getLong("revision"))
       .put("id", "checkpoints").put("folder", folder.getString("folder")).put("allow", new JSONArray().put(host)));
   context.getContentResolver().releasePersistableUriPermission(tree, Intent.FLAG_GRANT_READ_URI_PERMISSION);
   require(!share("checkpoints").getBoolean("available"), "Withdrawn source grant still available");
   boolean enableDenied = false;
   try { request("setServiceEnabled", new JSONObject().put("service", "checkpoints").put("incarnation", share.getString("incarnation")).put("enabled", true)); }
   catch (Exception denied) { enableDenied = true; }
   require(enableDenied, "Enabling a source without READ permission succeeded");
   signal("expired", peer); require(await("expired").getBoolean("success"), "Expired SAF grant exposed source data");
   checks.put("withdrawn persisted grant fails closed and requires reselection");
   require(inbox.toString().equals(inbox(request("receivingStatus", new JSONObject())).toString()), "Share operations changed inbox policy");
   checks.put("inbox folder and permissions unchanged");
   for (String id : new String[]{"checkpoints", "private"}) {
    status = request("sharesStatus", new JSONObject());
    request("removeShare", new JSONObject().put("id", id).put("expected_revision", status.getLong("revision")));
   }
   report.put("success", true).put("checks", checks).put("files", downloaded.getJSONObject("files"));
   test.finish(Activity.RESULT_OK, save(report, result));
  } catch (Throwable failure) {
   try { report.put("success", false).put("error", failure.toString()).put("checks", checks); } catch (Exception ignored) {}
   test.finish(Activity.RESULT_CANCELED, save(report, result));
  } finally { if (bridge != null) bridge.close(); }
 }
 private JSONObject share(String id) throws Exception {
  JSONArray rows = request("sharesStatus", new JSONObject()).getJSONArray("shares");
  for (int n=0; n<rows.length(); n++) if (id.equals(rows.getJSONObject(n).getString("id"))) return rows.getJSONObject(n);
  throw new Exception("Missing source share");
 }
 private static JSONObject inbox(JSONObject row) throws Exception {
  return new JSONObject().put("folder", row.getString("folder")).put("allow", row.getJSONArray("allow"));
 }
 private static JSONObject decode(byte[] frame) throws Exception {
  ByteBuffer bytes = ByteBuffer.wrap(frame); int length = bytes.getInt(); bytes.getInt(); byte[] metadata = new byte[length]; bytes.get(metadata);
  return new JSONObject(new String(metadata, StandardCharsets.UTF_8));
 }
 private JSONObject request(String method, JSONObject args) throws Exception { return bridge.request(method, args); }
 private void signal(String phase, String peer) throws Exception {
  try (FileOutputStream output = new FileOutputStream(new File(test.getTargetContext().getCacheDir(), "android-shares-ready.json"))) {
   output.write(new JSONObject().put("phase", phase).put("peer", peer).toString().getBytes(StandardCharsets.UTF_8));
  }
 }
 private JSONObject await(String phase) throws Exception {
  long until = SystemClock.elapsedRealtime() + 120000;
  File file = new File(test.getTargetContext().getCacheDir(), "android-shares-host-" + phase + ".json");
  while (SystemClock.elapsedRealtime() < until) {
   if (file.isFile()) try (FileInputStream input = new FileInputStream(file)) { return new JSONObject(new String(input.readAllBytes(), StandardCharsets.UTF_8)); }
   Thread.sleep(100);
  }
  throw new Exception("Desktop fixture timed out: " + phase);
 }
 private void click(String label, long timeout) throws Exception {
  long until = SystemClock.elapsedRealtime() + timeout;
  while (SystemClock.elapsedRealtime() < until) {
   AccessibilityNodeInfo root = test.getUiAutomation().getRootInActiveWindow();
   if (root != null) {
    java.util.ArrayDeque<AccessibilityNodeInfo> nodes = new java.util.ArrayDeque<>(); nodes.add(root);
    while (!nodes.isEmpty()) {
     AccessibilityNodeInfo node = nodes.removeFirst();
     CharSequence text = node.getText();
     if (text != null && label.equalsIgnoreCase(text.toString()) && node.isEnabled()) {
      AccessibilityNodeInfo action = node;
      for (int level=0; level<3 && action!=null; level++,action=action.getParent())
       if (action.performAction(AccessibilityNodeInfo.ACTION_CLICK)) return;
     }
     for (int child=0; child<node.getChildCount(); child++) { AccessibilityNodeInfo next=node.getChild(child); if(next!=null)nodes.add(next); }
    }
   }
   Thread.sleep(150);
  }
  throw new Exception("System picker missing: " + label);
 }
 private Bundle save(JSONObject report, Bundle result) {
  try (FileOutputStream output = new FileOutputStream(new File(test.getTargetContext().getCacheDir(), "android-shares-result.json"))) {
   output.write(report.toString().getBytes(StandardCharsets.UTF_8)); result.putString("result", report.toString());
  } catch (Exception failure) { result.putString("error", failure.toString()); }
  return result;
 }
 private static void require(boolean value, String message) throws Exception { if (!value) throw new Exception(message); }
}
