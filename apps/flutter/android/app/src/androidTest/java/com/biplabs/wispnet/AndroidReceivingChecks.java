package com.biplabs.wispnet;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Context;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.os.SystemClock;
import android.provider.DocumentsContract;
import android.view.accessibility.AccessibilityNodeInfo;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import org.json.JSONArray;
import org.json.JSONObject;

/** Disposable-emulator integration: real system folder grant, Binder, JNI and QUIC. */
final class AndroidReceivingChecks {
 private final Instrumentation test;
 private Bundle arguments;
 private NetworkBridge bridge;
 private final JSONArray checks = new JSONArray();
 AndroidReceivingChecks(Instrumentation test, Bundle args) { this.test = test; arguments = new Bundle(args); }
 void run() {
  JSONObject report = new JSONObject(); Bundle result = new Bundle();
  try {
   Context context = test.getTargetContext();
   String host = arguments.getString("host");
   Activity activity = test.startActivitySync(new Intent(context, FlutterMainActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
   context.startForegroundService(new Intent(context, AccessService.class));
   bridge = new NetworkBridge(context);
   request("setAccess", new JSONObject().put("enabled", true));
   request("pair", new JSONObject().put("code", arguments.getString("invitation")));
   for (int attempt = 0; attempt < 100; attempt++) {
    request("refreshHost", new JSONObject().put("host", host));
    JSONObject row = request("snapshot", new JSONObject()).getJSONObject("hosts").getJSONObject(host);
    if ("available".equals(row.optString("state"))) break;
    Thread.sleep(100);
   }
   JSONObject status = request("receivingStatus", new JSONObject());
   boolean paired = false;
   for (int n = 0; n < status.getJSONArray("clients").length(); n++)
    if (host.equals(status.getJSONArray("clients").getJSONObject(n).getString("peer"))) paired = true;
   require(paired, "Mutually paired desktop must appear in Android's receiver permissions");
   for (int n = 0; n < status.getJSONArray("allow").length(); n++)
    require(!host.equals(status.getJSONArray("allow").getString(n)), "Pairing must not grant Files access automatically");
   checks.put("mutual pairing without automatic Files grants");
   String folderName = arguments.getString("receiving_folder");
   require(folderName != null && folderName.matches("WispnetReceiveTest-[0-9a-f]{12}"), "Use a disposable receiving fixture folder");
   Uri tree = DocumentsContract.buildTreeDocumentUri("com.android.externalstorage.documents", "primary:Documents/" + folderName);
   Intent picker = new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE)
       .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION | Intent.FLAG_GRANT_PREFIX_URI_PERMISSION)
       .putExtra(DocumentsContract.EXTRA_INITIAL_URI, DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree)));
   test.runOnMainSync(() -> activity.startActivityForResult(picker, 106));
   click("Use this folder", 15000);
   click("Allow", 10000);
   Thread.sleep(500);
   context.getContentResolver().takePersistableUriPermission(tree, Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
   JSONObject folder = request("prepareReceivingFolder", new JSONObject().put("uri", tree.toString()));
   JSONObject config = new JSONObject().put("folder", folder.getString("folder")).put("allow", new JSONArray().put(host))
       .put("expected_revision", status.getLong("revision"));
   request("saveReceiving", config);
   status = request("receivingStatus", new JSONObject());
   require(status.getBoolean("available") && status.getJSONArray("allow").length() == 1, "Receiver folder and explicit grant must persist");
   boolean stale = false;
   try { request("saveReceiving", config); } catch (Exception conflict) { stale = true; }
   require(stale, "Stale owner revision must be rejected");
   checks.put("real Android system folder picker and persisted writable tree grant");
   checks.put("explicit sender permission with stale-revision rejection");
   test.runOnMainSync(() -> activity.moveTaskToBack(true));
   String peer = request("snapshot", new JSONObject()).getString("peer");
   signal("send", peer);
   JSONObject sent = await("send");
   require(sent.getBoolean("success"), sent.toString());
   for (String path : new String[]{"android-received.bin", "nested/資料.bin", "empty.bin"}) {
    // Read through SAF, as an app cannot assume raw shared-storage paths work.
    Uri uri = DocumentsContract.buildDocumentUriUsingTree(tree, "primary:Documents/" + folderName + "/" + path);
    MessageDigest hash = MessageDigest.getInstance("SHA-256"); long size = 0;
    try (java.io.InputStream input = context.getContentResolver().openInputStream(uri)) {
     require(input != null, "Missing published document"); byte[] buffer = new byte[65536]; int count;
     while ((count = input.read(buffer)) != -1) { hash.update(buffer, 0, count); size += count; }
    }
    JSONObject expected = sent.getJSONObject("files").getJSONObject(path);
    require(size == expected.getLong("size_bytes") && java.util.HexFormat.of().formatHex(hash.digest()).equals(expected.getString("sha256")), "Published document hash/size mismatch: " + path);
   }
   checks.put("background desktop-to-Android QUIC upload, Unicode subfolder and zero-byte file verified through SAF");
   checks.put("lost-response retry is idempotent and destination conflict preserves existing bytes");
   folderDownloadPublication(context, tree, folderName, folder.getString("folder"));
   request("setAccess", new JSONObject().put("enabled", false));
   signal("offline", peer); require(await("offline").getBoolean("success"), "Offline receiver accepted a file");
   request("setAccess", new JSONObject().put("enabled", true));
   signal("resume", peer); require(await("resume").getBoolean("success"), "Receiver failed to resume when online");
   checks.put("offline refusal and reenabled resumable receiving");
   status = request("receivingStatus", new JSONObject());
   request("saveReceiving", new JSONObject().put("folder", folder.getString("folder")).put("allow", new JSONArray()).put("expected_revision", status.getLong("revision")));
   signal("revoked", peer); require(await("revoked").getBoolean("success"), "Revoked peer could send");
   checks.put("Files grant revocation denies subsequent uploads");
   report.put("success", true).put("checks", checks).put("received_files", sent.getJSONObject("files"));
   test.finish(Activity.RESULT_OK, save(report, result));
  } catch (Throwable failure) {
   try { report.put("success", false).put("error", failure.toString()).put("checks", checks); } catch (Exception ignored) {}
   test.finish(Activity.RESULT_CANCELED, save(report, result));
  } finally { if (bridge != null) bridge.close(); }
 }
 private void folderDownloadPublication(Context context, Uri tree, String folderName, String root) throws Exception {
  AndroidReceiving receiving = new AndroidReceiving(context);
  String claim = digest("folder-download-owner:" + root), name = "downloaded-checkpoint";
  receiving.claimDownloadDirectory(root, name, claim, () -> false);
  receiving.claimDownloadDirectory(root, name, claim, () -> false);
  boolean collision = false;
  try { receiving.claimDownloadDirectory(root, name, digest("other-owner:" + root), () -> false); }
  catch (Native.Failure expected) { collision = "CONFLICT".equals(expected.code); }
  require(collision, "Folder download merged into another operation's destination");
  receiving.downloadDirectory(root, name + "/empty", () -> false);
  Uri empty = DocumentsContract.buildDocumentUriUsingTree(tree, "primary:Documents/" + folderName + "/" + name + "/empty");
  try (android.database.Cursor cursor = context.getContentResolver().query(empty,
      new String[]{DocumentsContract.Document.COLUMN_MIME_TYPE}, null, null, null)) {
   require(cursor != null && cursor.moveToFirst() && DocumentsContract.Document.MIME_TYPE_DIR.equals(cursor.getString(0)),
       "Empty downloaded folder was lost");
  }
  byte[] bytes = "lossless checkpoint 資料 😀".getBytes(StandardCharsets.UTF_8);
  String hash = java.util.HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(bytes));
  JSONObject publication = new JSONObject().put("transfer", digest("folder-download-file:" + root)).put("root", root)
      .put("path", name + "/nested/資料.bin").put("sha256", hash).put("size_bytes", bytes.length);
  File staged = new File(context.getCacheDir(), "folder-publication-fixture");
  try {
   try (FileOutputStream output = new FileOutputStream(staged)) { output.write(bytes); }
   try (android.os.ParcelFileDescriptor source = android.os.ParcelFileDescriptor.open(staged, android.os.ParcelFileDescriptor.MODE_READ_ONLY)) {
    require("ok".equals(receiving.publish(publication, source.getFd(), () -> false)), "Folder child publication failed");
   }
   require(receiving.downloadPublished(publication, () -> false), "Verified completed child was not resumable");
   Uri child = DocumentsContract.buildDocumentUriUsingTree(tree,
       "primary:Documents/" + folderName + "/" + name + "/nested/資料.bin");
   byte[] changed = bytes.clone(); changed[0] ^= 1;
   try (java.io.OutputStream output = context.getContentResolver().openOutputStream(child, "wt")) {
    require(output != null, "Cannot modify isolated publication fixture"); output.write(changed);
   }
   boolean modified = false;
   try { receiving.downloadPublished(publication, () -> false); }
   catch (Native.Failure expected) { modified = "CONFLICT".equals(expected.code); }
   require(modified, "Folder resume trusted a modified provider file with the same length");
  } finally { staged.delete(); }
  checks.put("folder download owns its destination, preserves empty directories and verifies completed SAF children before resume");
 }
 private static String digest(String value) throws Exception {
  return java.util.HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(value.getBytes(StandardCharsets.UTF_8)));
 }
 private JSONObject request(String method, JSONObject args) throws Exception { return bridge.request(method, args); }
 private void signal(String phase, String peer) throws Exception {
  File file = new File(test.getTargetContext().getCacheDir(), "android-receiving-ready.json");
  try (FileOutputStream output = new FileOutputStream(file)) {
   output.write(new JSONObject().put("phase", phase).put("peer", peer).toString().getBytes(StandardCharsets.UTF_8));
  }
 }
 private JSONObject await(String phase) throws Exception {
  long deadline = SystemClock.elapsedRealtime() + 120000;
  File file = new File(test.getTargetContext().getCacheDir(), "android-receiving-host-" + phase + ".json");
  while (SystemClock.elapsedRealtime() < deadline) {
   if (file.isFile()) try (FileInputStream input = new FileInputStream(file)) { return new JSONObject(new String(input.readAllBytes(), StandardCharsets.UTF_8)); }
   Thread.sleep(100);
  }
  throw new Exception("Timed out waiting for desktop: " + phase);
 }
 private void click(String label, long timeout) throws Exception {
  long until = SystemClock.elapsedRealtime() + timeout;
  while (SystemClock.elapsedRealtime() < until) {
   AccessibilityNodeInfo root = test.getUiAutomation().getRootInActiveWindow();
   if (root != null) for (AccessibilityNodeInfo node : root.findAccessibilityNodeInfosByText(label)) {
    if (node.isEnabled() && node.performAction(AccessibilityNodeInfo.ACTION_CLICK)) return;
   }
   Thread.sleep(150);
  }
  throw new Exception("System picker did not offer: " + label);
 }
 private Bundle save(JSONObject report, Bundle bundle) {
  try (FileOutputStream output = new FileOutputStream(new File(test.getTargetContext().getCacheDir(), "android-receiving-result.json"))) {
   output.write(report.toString().getBytes(StandardCharsets.UTF_8)); bundle.putString("result", report.toString());
  } catch (Exception failure) { bundle.putString("error", failure.toString()); }
  return bundle;
 }
 private static void require(boolean value, String message) throws Exception { if (!value) throw new Exception(message); }
}
