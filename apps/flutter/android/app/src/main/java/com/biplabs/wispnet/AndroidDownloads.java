package com.biplabs.wispnet;

import android.content.Context;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.os.SystemClock;
import android.system.Os;
import android.util.AtomicFile;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.ArrayDeque;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import org.json.JSONArray;
import org.json.JSONObject;

/** Pulls only requested files, then publishes verified data to a locally chosen SAF folder. */
final class AndroidDownloads implements AutoCloseable {
 private final Context context;
 private final ExecutorService work = Executors.newFixedThreadPool(2);
 private final Map<String, Job> jobs = new LinkedHashMap<>();
 private volatile boolean closed, stopped = true;

 private static final class Job {
  final String id = UUID.randomUUID().toString().replace("-", "") + UUID.randomUUID().toString().replace("-", "");
  final JSONObject request;
  final String root, name, binding;
  final File staged;
  final boolean recursive;
  JSONObject status;
  volatile String nativeId;
  volatile boolean cancelled;
  volatile long finishedAt;
  Job(JSONObject request, String root, String name, String binding, File staged, boolean recursive) throws Exception {
   this.request = request; this.root = root; this.name = name; this.binding = binding; this.staged = staged;
   this.recursive = recursive;
   status = new JSONObject().put("transfer", id).put("state", "running").put("phase", "queued")
       .put("verified_bytes", 0).put("size_bytes", 0);
  }
 }

 AndroidDownloads(Context context) { this.context = context.getApplicationContext(); }

 JSONObject prepare(Uri tree) throws Exception {
  JSONObject folder = new AndroidReceiving(context).prepare(tree);
  return new JSONObject().put("path", folder.getString("folder")).put("label", folder.getString("label"));
 }

 JSONObject list(JSONObject args) throws Exception {
  if (closed || stopped) throw new Exception("Go online before browsing shared files");
  JSONObject request = target(args).put("op", "file_list").put("path", args.optString("path", ""));
  if (args.has("cursor")) request.put("cursor", args.get("cursor"));
  return Native.request(request);
 }

 JSONObject get(JSONObject args) throws Exception {
  if (closed || stopped) throw new Exception("Go online before downloading files");
  String root = args.getString("local_path"), name = FileTransferRules.filename(args.getString("name"));
  if (args.has("recursive") && !(args.get("recursive") instanceof Boolean)) throw new Exception("Recursive must be true or false");
  boolean recursive = args.optBoolean("recursive", false);
  String path = DownloadTree.path(args.getString("path"), recursive);
  // A destination token only names a folder previously selected by this device's owner.
  String record = context.getSharedPreferences("receiving-folders", 0).getString(root, null);
  if (record == null) throw new SecurityException("Choose a download folder first");
  new AndroidReceiving(context).prepare(Uri.parse(new JSONObject(record).getString("uri")));
  JSONObject request = target(args).put("op", "file_get").put("path", path);
  if (args.has("resume") && !(args.get("resume") instanceof Boolean)) throw new Exception("Resume must be true or false");
  request.put("resume", args.optBoolean("resume", true));
  JSONArray bound = new JSONArray().put(root).put(name).put(request.getString("host"))
      .put(request.getString("service")).put(request.getString("incarnation")).put(path);
  // Preserve existing single-file resume bindings across this upgrade.
  if (recursive) bound.put("folder-v1");
  String binding = hash(bound.toString());
  File directory = new File(context.getFilesDir(), "downloads/" + binding);
  if (!directory.isDirectory() && !directory.mkdirs()) throw new Exception("Cannot prepare download storage");
  Os.chmod(directory.getAbsolutePath(), 0700);
  File staged = new File(directory, "verified-file");
  request.put("local_path", staged.getCanonicalPath());
  Job job = new Job(request, root, name, binding, staged, recursive);
  synchronized (jobs) {
   if (closed || stopped) throw new Exception("Networking stopped. Go online and try again.");
   long cutoff = SystemClock.elapsedRealtime() - 60 * 60 * 1000;
   jobs.entrySet().removeIf(entry -> entry.getValue().finishedAt > 0 && entry.getValue().finishedAt < cutoff);
   for (Job existing : jobs.values()) synchronized (existing) {
    if (existing.binding.equals(binding) && !finished(existing.status)) throw new Exception("This download is already active");
   }
   if (jobs.size() >= 64) throw new Exception("The transfer queue is full. Wait for a file to finish.");
   jobs.put(job.id, job);
   work.execute(() -> run(job));
  }
  return snapshot(job);
 }

 boolean contains(String id) { synchronized (jobs) { return jobs.containsKey(id); } }
 JSONArray snapshots() throws Exception {
  JSONArray result = new JSONArray();
  synchronized (jobs) {
   for (Job job : jobs.values()) {
    JSONObject request = target(job.request).put("path", job.request.getString("path"))
        .put("local_path", job.root).put("name", job.name).put("resume", true);
    if (job.recursive) request.put("recursive", true);
    result.put(snapshot(job).put("request", request).put("name", job.name));
   }
  }
  return result;
 }
 JSONObject status(String id) throws Exception { return snapshot(find(id)); }
 JSONObject cancel(String id) throws Exception {
  Job job = find(id);
  synchronized (job) { if (!finished(job.status)) job.cancelled = true; }
  return snapshot(job);
 }
 void cancelHost(String host) {
  ArrayList<Job> copy;
  synchronized (jobs) { copy = new ArrayList<>(jobs.values()); }
  for (Job job : copy) if (host == null || host.equals(job.request.optString("host")))
   try { cancel(job.id); } catch (Exception ignored) {}
 }
 void online() { if (!closed) stopped = false; }
 void stop() { stopped = true; cancelHost(null); }
 @Override public void close() { closed = true; stop(); work.shutdown(); }

 private void run(Job job) {
  try {
   check(job);
   if (job.recursive) { runFolder(job); return; }
   // This private artifact belongs only to this source/destination binding.
   // An explicit restart may discard it; provider documents are never replaced.
   if (!job.request.optBoolean("resume", true)
       && (job.staged.exists() || java.nio.file.Files.isSymbolicLink(job.staged.toPath()))
       && !job.staged.delete()) throw new Exception("Cannot discard the previous staged download");
   JSONObject nativeStatus = Native.request(job.request);
   job.nativeId = nativeStatus.getString("transfer");
   while (true) {
    update(job, nativeStatus);
    if (finished(nativeStatus)) break;
    if (job.cancelled || closed || stopped)
     Native.request(new JSONObject().put("op", "file_cancel").put("transfer", job.nativeId));
    Thread.sleep(300);
    nativeStatus = Native.request(new JSONObject().put("op", "file_status").put("transfer", job.nativeId));
   }
   if (!"complete".equals(nativeStatus.optString("state"))) {
    JSONObject error = nativeStatus.optJSONObject("error");
    if (error != null && "CONFLICT".equals(error.optString("code")) && job.staged.isFile())
     error.put("code", "STALE_VERSION").put("message", "The source changed after download. Restart to discard the previous staged copy.");
    synchronized (job) { job.status = new JSONObject(nativeStatus.toString()).put("transfer", job.id); }
    return;
   }
   check(job);
   JSONObject file = nativeStatus.getJSONObject("file");
   String receipt = hash(new JSONArray().put(job.binding).put(file.getString("sha256")).toString());
   synchronized (job) { job.status.put("phase", "saving"); }
   try (ParcelFileDescriptor source = ParcelFileDescriptor.open(job.staged, ParcelFileDescriptor.MODE_READ_ONLY)) {
    String result = new AndroidReceiving(context).publish(new JSONObject().put("transfer", receipt)
        .put("root", job.root).put("path", job.name).put("sha256", file.getString("sha256"))
        .put("size_bytes", file.getLong("size_bytes")), source.getFd(), () -> job.cancelled || closed || stopped);
    if (!"ok".equals(result)) throw new Native.Failure(
        "conflict".equals(result) ? "A file with this name already exists in the download folder" : "Cannot save this download",
        "conflict".equals(result) ? "CONFLICT" : "UNAVAILABLE");
   }
   synchronized (job) { job.status.put("state", "complete").put("phase", "complete").put("file", file); }
   job.staged.delete();
  } catch (Exception failure) {
   if (job.nativeId != null) try {
    Native.request(new JSONObject().put("op", "file_cancel").put("transfer", job.nativeId));
   } catch (Exception ignored) {}
   synchronized (job) {
    boolean cancelled = job.cancelled || closed || stopped;
    try { job.status.put("state", cancelled ? "cancelled" : "failed")
        .put("error", new JSONObject().put("code", cancelled ? "CANCELLED" : failure instanceof Native.Failure
            ? ((Native.Failure)failure).code : "UNAVAILABLE")
            .put("message", cancelled ? "Download paused. Select the same file and folder to resume."
                : failure.getMessage() == null ? "Download interrupted. Try again to resume." : failure.getMessage()));
    } catch (org.json.JSONException impossible) { throw new IllegalStateException(impossible); }
   }
  } finally { synchronized (job) { job.finishedAt = SystemClock.elapsedRealtime(); } }
 }

 /** Only one child is staged at a time; a large folder needs no second private
  * copy of its entire contents. The immutable plan binds resumes to versions. */
 private void runFolder(Job job) throws Exception {
  phase(job, "preparing");
  AtomicFile journal = new AtomicFile(new File(job.staged.getParentFile(), "folder-plan.json"));
  boolean resume = job.request.optBoolean("resume", true);
  JSONObject plan = null;
  if (resume && journal.getBaseFile().exists()) {
   if (journal.getBaseFile().length() > 16 * 1024 * 1024) throw new Exception("Download plan exceeds its size limit");
   try (FileInputStream input = journal.openRead()) {
    plan = new JSONObject(new String(input.readAllBytes(), StandardCharsets.UTF_8));
   }
   validatePlan(job, plan);
  }
  JSONArray listing = enumerate(job);
  if (plan == null) {
   JSONArray entries = new JSONArray(); long total = 0; int files = 0;
   for (int index = 0; index < listing.length(); index++) {
    check(job);
    JSONObject entry = new JSONObject(listing.getJSONObject(index).toString());
    if (!entry.getBoolean("is_dir")) {
     JSONObject info = stat(job, entry.getString("path"));
     long size = info.getLong("size_bytes");
     if (size > DownloadTree.MAX_BYTES - total) throw new Exception("This folder exceeds the download limit");
     total += size; files++; entry.put("file", info);
    }
    entries.put(entry);
   }
   plan = new JSONObject().put("version", 1).put("binding", job.binding).put("path", job.request.getString("path"))
       .put("entries", entries).put("files_total", files).put("size_bytes", total);
   // Catch membership or size changes that happened while versions were pinned.
   requireSameListing(listing, enumerate(job));
   byte[] bytes = plan.toString().getBytes(StandardCharsets.UTF_8);
   if (bytes.length > 16 * 1024 * 1024) throw new Exception("Download plan exceeds its size limit");
   FileOutputStream output = journal.startWrite();
   try { output.write(bytes); journal.finishWrite(output); }
   catch (Exception failure) { journal.failWrite(output); throw failure; }
  } else {
   requireSameListing(plan.getJSONArray("entries"), listing);
   JSONArray entries = plan.getJSONArray("entries");
   for (int index = 0; index < entries.length(); index++) {
    JSONObject entry = entries.getJSONObject(index);
    if (!entry.getBoolean("is_dir")) requireSameFile(entry.getJSONObject("file"), stat(job, entry.getString("path")));
   }
  }
  check(job);
  JSONArray entries = plan.getJSONArray("entries");
  long total = plan.getLong("size_bytes"), completed = 0; int files = plan.getInt("files_total"), done = 0;
  AndroidReceiving receiving = new AndroidReceiving(context);
  receiving.claimDownloadDirectory(job.root, job.name, job.binding, () -> cancelled(job));
  synchronized (job) { job.status.put("recursive", true).put("files_total", files).put("files_complete", 0)
      .put("size_bytes", total).put("verified_bytes", 0).put("phase", "transferring"); }
  for (int index = 0; index < entries.length(); index++) {
   check(job);
   JSONObject entry = entries.getJSONObject(index);
   String remote = entry.getString("path"), relative = relative(job, remote), destination = job.name + "/" + relative;
   if (entry.getBoolean("is_dir")) {
    receiving.downloadDirectory(job.root, destination, () -> cancelled(job)); continue;
   }
   JSONObject info = entry.getJSONObject("file");
   String receipt = hash(new JSONArray().put(job.binding).put(relative).put(info.getString("sha256")).toString());
   JSONObject publication = new JSONObject().put("transfer", receipt).put("root", job.root).put("path", destination)
       .put("sha256", info.getString("sha256")).put("size_bytes", info.getLong("size_bytes"));
   phase(job, "verifying");
   if (!receiving.downloadPublished(publication, () -> cancelled(job))) {
    File staged = new File(job.staged.getParentFile(), "child-" + hash(remote));
    if (!resume && (staged.exists() || java.nio.file.Files.isSymbolicLink(staged.toPath())) && !staged.delete())
     throw new Exception("Cannot discard the previous staged download");
    JSONObject request = target(job.request).put("op", "file_get").put("path", remote)
        .put("local_path", staged.getCanonicalPath()).put("resume", resume).put("expected_version", info.getString("file_version"));
    JSONObject actual = awaitFile(job, request, completed, total, done, files, relative).getJSONObject("file");
    requireSameFile(info, actual);
    phase(job, "saving");
    try (ParcelFileDescriptor source = ParcelFileDescriptor.open(staged, ParcelFileDescriptor.MODE_READ_ONLY)) {
     String result = receiving.publish(publication, source.getFd(), () -> cancelled(job));
     if (!"ok".equals(result)) throw new Native.Failure(
         "conflict".equals(result) ? "A file with this name already exists in the download folder" : "Cannot save this download",
         "conflict".equals(result) ? "CONFLICT" : "UNAVAILABLE");
    }
    if (!staged.delete()) throw new Exception("Cannot remove the verified staging file");
   }
   completed += info.getLong("size_bytes"); done++;
   synchronized (job) { job.status.put("verified_bytes", completed).put("files_complete", done); }
  }
  phase(job, "verifying");
  requireSameListing(plan.getJSONArray("entries"), enumerate(job));
  for (int index = 0; index < entries.length(); index++) {
   JSONObject entry = entries.getJSONObject(index);
   if (!entry.getBoolean("is_dir")) requireSameFile(entry.getJSONObject("file"), stat(job, entry.getString("path")));
  }
  check(job);
  synchronized (job) { job.status.put("state", "complete").put("phase", "complete")
      .put("verified_bytes", total).put("size_bytes", total).put("files_complete", files).put("files_total", files); }
 }

 private JSONObject awaitFile(Job job, JSONObject request, long completed, long total, int done, int files, String path) throws Exception {
  check(job);
  JSONObject status = Native.request(request);
  job.nativeId = status.getString("transfer");
  while (true) {
   synchronized (job) {
    job.status = new JSONObject(status.toString()).put("transfer", job.id).put("state", "running")
        .put("recursive", true).put("current_path", path).put("verified_bytes", completed + status.optLong("verified_bytes"))
        .put("size_bytes", total).put("files_total", files).put("files_complete", done);
    if ("complete".equals(status.optString("state"))) job.status.put("phase", "saving");
   }
   if (finished(status)) break;
   if (cancelled(job)) Native.request(new JSONObject().put("op", "file_cancel").put("transfer", job.nativeId));
   Thread.sleep(300);
   status = Native.request(new JSONObject().put("op", "file_status").put("transfer", job.nativeId));
  }
  job.nativeId = null;
  if (!"complete".equals(status.optString("state"))) {
   JSONObject error = status.optJSONObject("error");
   throw new Native.Failure(error == null ? "Download interrupted" : error.optString("message", "Download interrupted"),
       error == null ? "UNAVAILABLE" : error.optString("code", "UNAVAILABLE"));
  }
  check(job); return status;
 }

 private JSONArray enumerate(Job job) throws Exception {
  DownloadTree tree = new DownloadTree(job.request.getString("path"));
  ArrayDeque<String> folders = new ArrayDeque<>(); folders.add(tree.root);
  JSONArray result = new JSONArray();
  while (!folders.isEmpty()) {
   String parent = folders.removeFirst(), cursor = null;
   do {
    check(job);
    JSONObject request = target(job.request).put("op", "file_list").put("path", parent);
    if (cursor != null) request.put("cursor", cursor);
    JSONObject page = Native.request(request); JSONArray entries = page.getJSONArray("entries");
    if (entries.length() > 256) throw new SecurityException("Invalid shared folder page");
    String previous = cursor;
    for (int index = 0; index < entries.length(); index++) {
     check(job); JSONObject row = entries.getJSONObject(index); String name = row.getString("name");
     if (previous != null && DownloadTree.compareNames(name, previous) <= 0) throw new SecurityException("Invalid shared folder ordering");
     previous = name;
     DownloadTree.Entry entry = tree.add(parent, name, row.getString("path"), row.getBoolean("is_dir"), row.getLong("size_bytes"));
     result.put(new JSONObject().put("name", name).put("path", entry.path).put("is_dir", entry.directory).put("size_bytes", entry.size));
     if (entry.directory) folders.addLast(entry.path);
    }
    String next = page.isNull("next_cursor") ? null : page.getString("next_cursor");
    if (next != null && (entries.length() == 0 || !next.equals(previous) || (cursor != null && DownloadTree.compareNames(next, cursor) <= 0)))
     throw new SecurityException("Invalid shared folder cursor");
    cursor = next;
   } while (cursor != null);
  }
  return result;
 }

 private JSONObject stat(Job job, String path) throws Exception {
  check(job);
  JSONObject status = Native.request(target(job.request).put("op", "file_stat").put("path", path));
  job.nativeId = status.getString("transfer");
  synchronized (job) { job.status.put("current_path", relative(job, path)); }
  while (!finished(status)) {
   if (cancelled(job)) Native.request(new JSONObject().put("op", "file_cancel").put("transfer", job.nativeId));
   Thread.sleep(300);
   status = Native.request(new JSONObject().put("op", "file_status").put("transfer", job.nativeId));
  }
  job.nativeId = null;
  if (!"complete".equals(status.optString("state"))) {
   JSONObject error = status.optJSONObject("error");
   throw new Native.Failure(error == null ? "Cannot inspect the shared file" : error.optString("message", "Cannot inspect the shared file"),
       error == null ? "UNAVAILABLE" : error.optString("code", "UNAVAILABLE"));
  }
  JSONObject info = status.getJSONObject("file");
  long size = info.getLong("size_bytes");
  if (!path.equals(info.getString("path")) || size < 0 || size > DownloadTree.MAX_BYTES
      || !info.getString("sha256").matches("[0-9a-f]{64}") || info.getString("file_version").isEmpty()
      || info.getString("file_version").length() > 4096) throw new SecurityException("Invalid shared file version");
  check(job); return info;
 }

 private static void validatePlan(Job job, JSONObject plan) throws Exception {
  if (plan.getInt("version") != 1 || !job.binding.equals(plan.getString("binding"))
      || !job.request.getString("path").equals(plan.getString("path"))) throw new SecurityException("Download plan changed");
  DownloadTree tree = new DownloadTree(plan.getString("path"));
  JSONArray entries = plan.getJSONArray("entries"); long size = 0; int files = 0;
  for (int index = 0; index < entries.length(); index++) {
   JSONObject entry = entries.getJSONObject(index); String path = entry.getString("path");
   int slash = path.lastIndexOf('/'); String parent = slash < 0 ? "" : path.substring(0, slash);
   tree.add(parent, entry.getString("name"), path, entry.getBoolean("is_dir"), entry.getLong("size_bytes"));
   if (!entry.getBoolean("is_dir")) {
    JSONObject info = entry.getJSONObject("file"); long bytes = info.getLong("size_bytes");
    if (!path.equals(info.getString("path")) || bytes < 0 || bytes > DownloadTree.MAX_BYTES - size
        || !info.getString("sha256").matches("[0-9a-f]{64}") || info.getString("file_version").isEmpty()
        || info.getString("file_version").length() > 4096) throw new SecurityException("Invalid download plan");
    size += bytes; files++;
   }
  }
  if (size != plan.getLong("size_bytes") || files != plan.getInt("files_total")) throw new SecurityException("Invalid download totals");
 }

 private static void requireSameListing(JSONArray left, JSONArray right) throws Exception {
  java.util.TreeMap<String, String> a = listingMap(left), b = listingMap(right);
  if (!a.equals(b)) throw new Native.Failure("The shared folder changed. Restart the download to select its current contents.", "STALE_VERSION");
 }
 private static java.util.TreeMap<String, String> listingMap(JSONArray entries) throws Exception {
  java.util.TreeMap<String, String> result = new java.util.TreeMap<>();
  for (int index = 0; index < entries.length(); index++) {
   JSONObject entry = entries.getJSONObject(index);
   String signature = new JSONArray().put(entry.getBoolean("is_dir")).put(entry.getLong("size_bytes")).toString();
   if (result.put(entry.getString("path"), signature) != null) throw new SecurityException("Duplicate download path");
  }
  return result;
 }
 private static void requireSameFile(JSONObject expected, JSONObject actual) throws Exception {
  for (String key : new String[]{"path", "sha256", "file_version", "size_bytes"})
   if (!expected.get(key).equals(actual.get(key))) throw new Native.Failure("A shared file changed. Restart the folder download.", "STALE_VERSION");
 }
 private static String relative(Job job, String path) throws Exception {
  String base = job.request.getString("path"), prefix = base.isEmpty() ? "" : base + "/";
  if (!path.startsWith(prefix)) throw new SecurityException("Invalid download path");
  return DownloadTree.path(path.substring(prefix.length()), false);
 }
 private static void phase(Job job, String value) throws Exception { synchronized (job) { job.status.put("phase", value); } }
 private boolean cancelled(Job job) { return job.cancelled || closed || stopped; }

 private static void update(Job job, JSONObject status) throws Exception {
  synchronized (job) {
   job.status = new JSONObject(status.toString()).put("transfer", job.id);
   // Completion includes publication into the chosen provider, not just private staging.
   if ("complete".equals(job.status.optString("state"))) job.status.put("state", "running").put("phase", "saving");
  }
 }
 private void check(Job job) throws Exception {
  if (job.cancelled || closed || stopped) throw new Exception("Download paused");
 }
 private Job find(String id) throws Exception {
  synchronized (jobs) { Job job = jobs.get(id); if (job != null) return job; }
  throw new Exception("Download handle unavailable. Select the same file and folder to resume.");
 }
 private static boolean finished(JSONObject status) {
  return java.util.Set.of("complete", "failed", "cancelled").contains(status.optString("state"));
 }
 private static JSONObject snapshot(Job job) throws Exception { synchronized (job) { return new JSONObject(job.status.toString()); } }
 private static JSONObject target(JSONObject args) throws Exception {
  String host = args.getString("host"), service = args.getString("service"), incarnation = args.getString("incarnation");
  if (!host.matches("[0-9a-f]{64}") || !incarnation.matches("[0-9a-f]{64}")
      || !service.matches("[a-z0-9][a-z0-9._-]{0,63}")) throw new Exception("Invalid shared files target. Refresh this device.");
  return new JSONObject().put("host", host).put("service", service).put("incarnation", incarnation);
 }
 private static String hash(String value) throws Exception {
  byte[] bytes = MessageDigest.getInstance("SHA-256").digest(value.getBytes(StandardCharsets.UTF_8));
  StringBuilder result = new StringBuilder();
  for (byte item : bytes) result.append(String.format(java.util.Locale.ROOT, "%02x", item & 255));
  return result.toString();
 }
}
