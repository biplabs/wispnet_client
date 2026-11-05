package com.biplabs.wispnet;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Context;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.os.SystemClock;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import org.json.JSONArray;
import org.json.JSONObject;

/** Opt-in real SAF/native transfer checks against an isolated owner fixture. */
public final class FilesTransferInstrumentation extends Instrumentation {
 private Bundle arguments;
 private NetworkBridge bridge;
 private JSONObject target;
 private final JSONArray completed = new JSONArray(), checks = new JSONArray();
 private String workspace;

 @Override public void onCreate(Bundle arguments) { this.arguments = arguments == null ? Bundle.EMPTY : new Bundle(arguments); start(); }
 private void shareSetup() {
  Bundle result = new Bundle();
  try {
   Context context = getTargetContext();
   bridge = new NetworkBridge(context);
   String host = arguments.getString("host");
   if ("true".equals(arguments.getString("cleanup"))) {
    request("removeHost", new JSONObject().put("host", host));
   } else {
    context.startForegroundService(new Intent(context, AccessService.class));
    request("setAccess", new JSONObject().put("enabled", true));
    request("pair", new JSONObject().put("code", arguments.getString("invitation")));
    boolean ready = false;
    for (int attempt = 0; attempt < 60 && !ready; attempt++) {
     request("refreshHost", new JSONObject().put("host", host));
     JSONObject record = request("snapshot", new JSONObject()).getJSONObject("hosts").getJSONObject(host);
     JSONObject catalog = record.optJSONObject("catalog");
     if (catalog != null) {
      JSONArray services = catalog.getJSONArray("services");
      for (int n = 0; n < services.length(); n++) if ("files".equals(services.getJSONObject(n).optString("kind"))) ready = true;
     }
     if (!ready) SystemClock.sleep(500);
    }
    require(ready, "Share fixture did not become ready");
   }
   result.putString("share", "ready"); finish(Activity.RESULT_OK, result);
  } catch (Exception failure) {
   result.putString("error", failure.toString()); finish(Activity.RESULT_CANCELED, result);
  } finally { if (bridge != null) bridge.close(); }
 }
 @Override public void onStart() {
  if ("true".equals(arguments.getString("ownerChecks"))) { new AndroidOwnerChecks(this).run(); return; }
  if ("true".equals(arguments.getString("cameraChecks"))) { AndroidQrCameraChecks.run(this, "true".equals(arguments.getString("cameraLive"))); return; }
  if ("true".equals(arguments.getString("namedInboxes"))) { new AndroidInboxChecks(this, arguments).run(); return; }
  if ("true".equals(arguments.getString("sourceShares"))) { new AndroidSharesChecks(this, arguments).run(); return; }
  if ("true".equals(arguments.getString("shareSetup"))) { shareSetup(); return; }
  if ("true".equals(arguments.getString("receiving"))) { new AndroidReceivingChecks(this, arguments).run(); return; }
  Bundle result = new Bundle(); JSONObject report = new JSONObject();
  try {
   String host = arguments.getString("host"), invitation = arguments.getString("invitation");
   workspace = arguments.getString("workspace", "training");
   require(host != null && host.matches("[0-9a-f]{64}") && invitation != null, "Provide an isolated host and invitation");
   Context context = getTargetContext();
   startActivitySync(new Intent(context, FlutterMainActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
   waitForIdleSync();
   context.startForegroundService(new Intent(context, AccessService.class));
   bridge = new NetworkBridge(context);
   request("setAccess", new JSONObject().put("enabled", true));
   request("pair", new JSONObject().put("code", invitation));
   String incarnation = null;
   JSONObject record = new JSONObject();
   for (int attempt = 0; attempt < 100 && incarnation == null; attempt++) {
    request("refreshHost", new JSONObject().put("host", host));
    JSONObject snapshot = request("snapshot", new JSONObject());
    record = snapshot.getJSONObject("hosts").getJSONObject(host);
    JSONObject catalog = record.optJSONObject("catalog");
    if (catalog != null) {
     JSONArray services = catalog.getJSONArray("services");
     for (int n = 0; n < services.length(); n++) if ("files".equals(services.getJSONObject(n).optString("kind"))) incarnation = services.getJSONObject(n).getString("incarnation");
    }
    if (incarnation == null) Thread.sleep(100);
   }
   require(incarnation != null, "Fixture must grant Files access: " + record.optString("status"));
   target = new JSONObject().put("host", host).put("incarnation", incarnation).put("workspace", workspace);
   JSONArray folders = request("fileWorkspaces", target).getJSONArray("workspaces");
   boolean listed = false; for (int n = 0; n < folders.length(); n++) if (workspace.equals(folders.getString(n))) listed = true;
   require(listed, "Receiving folder was not discovered"); checks.put("authenticated receiving-folder discovery");
   directCancellationResume();
   individualAndQueuedCancellation();
   completeFile("pipe", 8L * 1024 * 1024, 73, "android-pipe.bin");
   completeFile("slice", 1024L * 1024 + 19, 151, "android-slice.bin");
   JSONObject history = request("fileList", new JSONObject());
   require(history.getJSONArray("transfers").length() >= completed.length(), "History lost its native jobs");
   checks.put("native per-file history");
   report.put("success", true).put("files", completed).put("checks", checks);
   finish(Activity.RESULT_OK, saveResult(report, result));
  } catch (Throwable failure) {
   try { report.put("success", false).put("error", failure.toString()).put("files", completed).put("checks", checks); }
   catch (Exception ignored) {}
   finish(Activity.RESULT_CANCELED, saveResult(report, result));
  } finally {
   if (bridge != null) bridge.close();
   try { getTargetContext().getContentResolver().call(Uri.parse("content://" + FilesTestProvider.AUTHORITY), "clean", null, null); }
   catch (Exception ignored) {}
  }
 }

 private void directCancellationResume() throws Exception {
  long bytes = Long.parseLong(arguments.getString("direct_mib", "128")) * 1024 * 1024;
  Uri uri = grant("direct", bytes, 17);
  JSONObject first = send(uri, "android-fd-resumed.bin");
  long until = SystemClock.elapsedRealtime() + 120000;
  JSONObject progress = first;
  while (SystemClock.elapsedRealtime() < until) {
   progress = status(first);
   if (progress.optLong("verified_bytes") > 0 && !terminal(progress)) break;
   require(!terminal(progress), "Direct fixture finished before its partial cancellation could be observed: " + progress);
   Thread.sleep(50);
  }
  require(progress.optLong("verified_bytes") > 0 && progress.optLong("verified_bytes") < bytes, "No partial native progress observed");
  request("fileCancel", handle(first));
  JSONObject cancelled = await(first, 30000);
  require("cancelled".equals(cancelled.optString("state")), "Native transfer did not cancel individually: " + cancelled);
  checks.put("individual native transfer cancellation after durable progress");
  JSONObject resumed = await(send(uri, "android-fd-resumed.bin"), 180000);
  verifyComplete(resumed, "android-fd-resumed.bin", bytes, 17);
  checks.put("descriptor re-selection resumes the original native upload");
 }

 private void individualAndQueuedCancellation() throws Exception {
  Uri slow = grant("slow", 64L * 1024 * 1024, 43);
  JSONObject a = send(slow, "android-copy-cancelled.bin");
  JSONObject b = send(grant("slow", 64L * 1024 * 1024, 44), "android-copy-cancelled-2.bin");
  long until = SystemClock.elapsedRealtime() + 30000;
  while (SystemClock.elapsedRealtime() < until) {
   JSONObject left = status(a), right = status(b);
   if ("copying".equals(left.optString("phase")) && left.optLong("copied_bytes") > 0 && "copying".equals(right.optString("phase"))) break;
   require(!terminal(left) && !terminal(right), "Slow provider unexpectedly finished"); Thread.sleep(20);
  }
  require("copying".equals(status(a).optString("phase")), "No bounded provider preparation observed");
  JSONObject queued = send(grant("direct", 1024 * 1024, 19), "android-queued-cancelled.bin");
  request("fileCancel", handle(queued));
  require("cancelled".equals(await(queued, 10000).getString("state")), "Queued cancellation failed");
  request("fileCancel", handle(a));
  request("fileCancel", handle(b));
  require("cancelled".equals(await(a, 30000).getString("state")), "Provider preparation did not cancel");
  require("cancelled".equals(await(b, 30000).getString("state")), "Second provider preparation did not cancel");
  checks.put("queued file and bounded provider reads cancel individually");
  JSONObject isolated = send(grant("slow", 64L * 1024 * 1024, 83), "android-isolated-cancelled.bin");
  JSONObject other = send(grant("direct", 8L * 1024 * 1024, 91), "android-other-continues.bin");
  request("fileCancel", handle(isolated));
  require("cancelled".equals(await(isolated, 30000).getString("state")), "Isolated cancellation failed");
  verifyComplete(await(other, 60000), "android-other-continues.bin", 8L * 1024 * 1024, 91);
  checks.put("cancelling one file leaves the other transfer running");
 }

 private void completeFile(String kind, long bytes, int seed, String path) throws Exception {
  verifyComplete(await(send(grant(kind, bytes, seed), path), 60000), path, bytes, seed);
  checks.put(kind.equals("slice") ? "asset descriptor offsets and declared length honored" : "nonseekable provider bounded-copy fallback");
 }
 private Uri grant(String kind, long bytes, int seed) {
  Uri uri = FilesTestProvider.fixture(kind, bytes, seed);
  getTargetContext().getContentResolver().call(Uri.parse("content://" + FilesTestProvider.AUTHORITY), "grant", uri.toString(), null);
  return uri;
 }
 private JSONObject send(Uri uri, String path) throws Exception {
  return request("fileSend", new JSONObject(target.toString()).put("local_path", uri.toString()).put("remote_path", path).put("resume", true));
 }
 private JSONObject status(JSONObject job) throws Exception { return request("fileStatus", handle(job)); }
 private JSONObject handle(JSONObject job) throws Exception { return new JSONObject().put("transfer", job.getString("transfer")); }
 private JSONObject request(String method, JSONObject value) throws Exception { return bridge.request(method, value); }
 private static boolean terminal(JSONObject job) { return !"running".equals(job.optString("state")); }
 private JSONObject await(JSONObject job, long timeoutMillis) throws Exception {
  long until = SystemClock.elapsedRealtime() + timeoutMillis;
  while (SystemClock.elapsedRealtime() < until) { JSONObject current = status(job); if (terminal(current)) return current; Thread.sleep(50); }
  throw new AssertionError("Transfer timed out: " + status(job));
 }
 private void verifyComplete(JSONObject job, String path, long bytes, int seed) throws Exception {
  require("complete".equals(job.optString("state")), "File did not complete: " + job);
  String expected = FilesTestProvider.expectedSha256(bytes, seed);
  require(job.getLong("verified_bytes") == bytes && job.getLong("size_bytes") == bytes, "File size mismatch");
  require(expected.equals(job.getJSONObject("file").getString("sha256")), "Native file digest differs from fixture");
  completed.put(new JSONObject().put("path", path).put("size_bytes", bytes).put("sha256", expected));
 }
 private static void require(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
 private Bundle saveResult(JSONObject report, Bundle result) {
  result.putString("stream", report.toString() + "\n"); result.putString("results", report.toString());
  try (FileOutputStream out = new FileOutputStream(new File(getTargetContext().getCacheDir(), "android-files-result.json"))) {
   out.write(report.toString().getBytes(StandardCharsets.UTF_8));
  } catch (Exception failure) { result.putString("save_error", failure.toString()); }
  return result;
 }
}
