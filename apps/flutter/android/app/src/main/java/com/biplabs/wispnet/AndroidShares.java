package com.biplabs.wispnet;

import android.content.Context;
import android.content.SharedPreferences;
import android.content.res.AssetFileDescriptor;
import android.database.Cursor;
import android.net.Uri;
import android.os.CancellationSignal;
import android.provider.DocumentsContract;
import android.provider.DocumentsContract.Document;
import android.system.Os;
import java.io.Closeable;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.TreeMap;
import java.util.UUID;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;
import org.json.JSONArray;
import org.json.JSONObject;

/** A selected SAF tree is a read-only capability. Marker directories contain no
 * source files; URI grants and source paths remain in the network process. */
final class AndroidShares {
 private static final int MAX_CHUNK = 4 * 1024 * 1024;
 private static final int MAX_METADATA = 16 * 1024;
 private static final long MAX_FILE = 1L << 40;
 private static final int MAX_SCAN = 100000;
 private static final String[] COLUMNS = {Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME,
     Document.COLUMN_MIME_TYPE, Document.COLUMN_SIZE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_FLAGS};
 private static final Object CONFIG_LOCK = new Object();
 private static final Map<String, Version> VERSIONS = new LinkedHashMap<>(128, .75f, true);
 private static final ScheduledExecutorService CANCELLATION = Executors.newScheduledThreadPool(4, runnable -> {
  Thread thread = new Thread(runnable, "wispnet-share-cancellation"); thread.setDaemon(true); return thread;
 });
 private final Context context;
 private final SharedPreferences roots;
 AndroidShares(Context context) {
  this.context = context.getApplicationContext();
  roots = context.getSharedPreferences("shared-folders", Context.MODE_PRIVATE);
 }

 JSONObject prepare(Uri tree) throws Exception {
  if (!"content".equals(tree.getScheme()) || !DocumentsContract.isTreeUri(tree))
   throw new SecurityException("Choose a shared folder using Android's folder picker.");
  requireGrant(tree);
  Row row = metadata(tree, document(tree), null);
  if (!row.directory) throw new SecurityException("Choose a folder to share.");
  synchronized (CONFIG_LOCK) {
   for (String marker : roots.getAll().keySet()) {
    JSONObject entry = new JSONObject(roots.getString(marker, "{}"));
    if (tree.toString().equals(entry.optString("uri"))) return new JSONObject().put("folder", marker).put("label", row.name);
   }
   // This sibling of protected native state is an opaque policy marker only.
   File marker = new File(context.getFilesDir(), "shared-folders/" + UUID.randomUUID());
   if (!marker.mkdirs()) throw new IOException("Cannot prepare shared folder.");
   Os.chmod(marker.getAbsolutePath(), 0700);
   String path = marker.getCanonicalPath();
   if (!roots.edit().putString(path, new JSONObject().put("uri", tree.toString()).put("label", row.name).toString()).commit())
    throw new IOException("Cannot save shared folder.");
   return new JSONObject().put("folder", path).put("label", row.name);
  }
 }

 JSONObject status() throws Exception {
  JSONObject nativeStatus = Native.command("local_shares_status");
  JSONArray result = new JSONArray(), services = nativeStatus.getJSONArray("shares");
  JSONObject configured = nativeStatus.getJSONObject("roots");
  for (int index = 0; index < services.length(); index++) {
   JSONObject service = new JSONObject(services.getJSONObject(index).toString());
   String marker = configured.optString(service.getString("id"), "");
   JSONObject entry = new JSONObject(roots.getString(marker, "{}"));
   boolean available = false;
   try { validate(marker); available = true; } catch (Exception unavailable) { /* show reselection action */ }
   service.put("folder", marker).put("label", entry.optString("label", "Shared folder"))
       .put("available", available);
   service.remove("backend"); result.put(service);
  }
  return new JSONObject().put("revision", nativeStatus.getLong("revision"))
      .put("clients", nativeStatus.getJSONArray("clients")).put("shares", result);
 }

 JSONObject configure(JSONObject args) throws Exception {
  String marker = args.getString("folder");
  validate(marker);
  JSONObject request = new JSONObject().put("op", "local_share_configure")
      .put("expected_revision", args.getLong("expected_revision"))
      .put("id", args.getString("id")).put("root", marker).put("allow", args.getJSONArray("allow"));
  if (args.has("new_id")) request.put("new_id", args.getString("new_id"));
  return Native.request(request);
 }
 JSONObject remove(JSONObject args) throws Exception {
  String id = args.getString("id");
  JSONObject status = Native.command("local_shares_status");
  boolean found = false;
  JSONArray services = status.getJSONArray("shares");
  for (int index = 0; index < services.length(); index++) if (id.equals(services.getJSONObject(index).getString("id"))) found = true;
  if (!found) throw new SecurityException("Read-only share unavailable.");
  return Native.request(new JSONObject().put("op", "local_share_remove")
      .put("expected_revision", args.getLong("expected_revision")).put("id", id));
 }
 void requireAvailable(String id) throws Exception {
  JSONObject status = Native.command("local_shares_status");
  String marker = status.getJSONObject("roots").optString(id, "");
  validate(marker);
 }
 private Uri validate(String marker) throws Exception { return validate(marker, null); }
 private Uri validate(String marker, Operation operation) throws Exception {
  if (!roots.contains(marker)) throw new SecurityException("Choose the shared folder again.");
  Uri tree = Uri.parse(new JSONObject(roots.getString(marker, "{}")).getString("uri"));
  requireGrant(tree);
  if (!metadata(tree, document(tree), operation == null ? null : operation.signal).directory) throw new SecurityException("Shared folder unavailable. Choose it again.");
  return tree;
 }
 private void requireGrant(Uri tree) {
  for (android.content.UriPermission permission : context.getContentResolver().getPersistedUriPermissions())
   if (tree.equals(permission.getUri()) && permission.isReadPermission()) return;
  throw new SecurityException("Folder access expired. Choose the shared folder again.");
 }

 /** Existing Files frames carry bytes directly through JNI, never through UI/Binder JSON. */
 byte[] request(JSONObject envelope, BooleanSupplier cancelled) {
  try (Operation operation = new Operation(cancelled)) {
   operation.check();
   String root = envelope.getString("root");
   Uri tree = validate(root, operation);
   JSONObject request = envelope.getJSONObject("request");
   String path = request.getString("path");
   parts(path);
   String op = request.getString("op");
   switch (op) {
    case "share_list": return list(tree, path, request.isNull("cursor") ? "" : request.getString("cursor"), operation);
    case "share_stat": return stat(root, tree, path, operation);
    case "share_read_range": return read(root, tree, path, request, operation);
    default: return error("PERMISSION_DENIED");
   }
  } catch (Failure failure) { return error(failure.code); }
  catch (SecurityException denied) { return error("PERMISSION_DENIED"); }
  catch (java.io.FileNotFoundException missing) { return error("NOT_FOUND"); }
  catch (Exception unavailable) { return error("UNAVAILABLE"); }
 }

 private byte[] list(Uri tree, String path, String cursor, Operation operation) throws Exception {
  if (!cursor.isEmpty()) { String[] components = parts(cursor); if (components.length != 1) throw new Failure("INVALID_ARGUMENT"); }
  Row parent = resolve(tree, path, operation);
  if (!parent.directory) throw new Failure("PATH_DENIED");
  Uri children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, parent.id);
  TreeMap<String, JSONObject> entries = new TreeMap<>(AndroidShares::compareNames);
  boolean more = false;
  try (Cursor rows = context.getContentResolver().query(children, COLUMNS, null, null, null, operation.signal)) {
   if (rows == null) throw new IOException("Shared folder unavailable");
   int scanned = 0;
   while (rows.moveToNext()) {
    operation.check(); if (++scanned > MAX_SCAN) throw new Failure("RESOURCE_LIMIT");
    Row child = new Row(tree, rows);
    if (!validName(child.name) || child.virtual || (!cursor.isEmpty() && compareNames(child.name, cursor) <= 0)) continue;
    String relative = path.isEmpty() ? child.name : path + "/" + child.name;
    parts(relative);
    JSONObject entry = new JSONObject().put("name", child.name).put("path", relative)
        .put("is_dir", child.directory).put("size_bytes", child.directory ? 0 : Math.max(0, child.size));
    if (entries.put(child.name, entry) != null) throw new Failure("PATH_DENIED");
    if (entries.size() > 33) { entries.pollLastEntry(); more = true; }
   }
  }
  JSONArray page = new JSONArray(); String last = null;
  for (Map.Entry<String, JSONObject> entry : entries.entrySet()) {
   if (page.length() >= 32) { more = true; break; }
   JSONArray proposed = new JSONArray(page.toString()).put(entry.getValue());
   if (new JSONObject().put("type", "entries").put("entries", proposed).put("next_cursor", entry.getKey())
       .toString().getBytes(StandardCharsets.UTF_8).length > MAX_METADATA - 64) { more = true; break; }
   page = proposed; last = entry.getKey();
  }
  if (more && last == null) throw new Failure("RESOURCE_LIMIT");
  return frame(new JSONObject().put("type", "entries").put("entries", page)
      .put("next_cursor", more ? last : JSONObject.NULL), new byte[0]);
 }

 private byte[] stat(String root, Uri tree, String path, Operation operation) throws Exception {
  Row before = resolve(tree, path, operation);
  if (before.directory || before.virtual) throw new Failure("PATH_DENIED");
  MessageDigest digest = MessageDigest.getInstance("SHA-256"); long count = 0;
  try (AssetFileDescriptor asset = open(before.uri, operation); InputStream input = asset.createInputStream()) {
   operation.attach(input);
   byte[] buffer = new byte[256 * 1024]; int read;
   while ((read = input.read(buffer)) != -1) {
    operation.check(); if (read == 0) throw new Failure("UNAVAILABLE"); count += read; if (count > MAX_FILE) throw new Failure("RESOURCE_LIMIT");
    digest.update(buffer, 0, read);
   }
  } finally { operation.attach(null); }
  Row after = resolve(tree, path, operation);
  if (!before.same(after) || (before.size >= 0 && before.size != count)) throw new Failure("STALE_VERSION");
  String hash = java.util.HexFormat.of().formatHex(digest.digest());
  String version = sha256((root + "\n" + path + "\n" + before.id + "\n" + count + "\n" + before.modified + "\n" + hash).getBytes(StandardCharsets.UTF_8));
  synchronized (VERSIONS) {
   VERSIONS.put(root + "\n" + path, new Version(before, version, count));
   while (VERSIONS.size() > 128) VERSIONS.remove(VERSIONS.keySet().iterator().next());
  }
  return frame(new JSONObject().put("type", "file").put("file", new JSONObject()
      .put("path", path).put("size_bytes", count).put("sha256", hash).put("file_version", version)), new byte[0]);
 }

 private byte[] read(String root, Uri tree, String path, JSONObject request, Operation operation) throws Exception {
  long offset = request.getLong("offset"), maximum = request.getLong("max_bytes");
  if (offset < 0 || maximum < 1 || maximum > MAX_CHUNK) throw new Failure("INVALID_ARGUMENT");
  Version version;
  synchronized (VERSIONS) { version = VERSIONS.get(root + "\n" + path); }
  // A folder manifest can pin more files than this bounded provider cache holds.
  // Rebuild an evicted entry using the same cancellable, full-content stat; the
  // exact caller-pinned version still has to match before any bytes are sent.
  if (version == null) {
   stat(root, tree, path, operation);
   synchronized (VERSIONS) { version = VERSIONS.get(root + "\n" + path); }
  }
  Row before = resolve(tree, path, operation);
  if (version == null || !version.value.equals(request.getString("expected_version")) || !version.row.same(before)) throw new Failure("STALE_VERSION");
  if (offset > version.size) throw new Failure("INVALID_OFFSET");
  byte[] bytes = new byte[(int)Math.min(maximum, version.size - offset)];
  try (AssetFileDescriptor asset = open(before.uri, operation); InputStream input = asset.createInputStream()) {
   operation.attach(input);
   // File streams seek in constant time; providers exposing pipes are skipped
   // using bounded buffers. Neither path copies the whole source into storage.
   long remaining = offset;
   if (input instanceof java.io.FileInputStream) {
    try { ((java.io.FileInputStream)input).getChannel().position(Math.addExact(asset.getStartOffset(), offset)); remaining = 0; }
    catch (IOException nonSeekable) { /* A stream-only provider uses bounded skip buffers below. */ }
   }
   byte[] skip = new byte[256 * 1024];
   while (remaining > 0) {
    operation.check(); long skipped;
    try { skipped = input.skip(remaining); } catch (IOException nonSeekable) { skipped = 0; }
    if (skipped > remaining || skipped < 0) throw new Failure("UNAVAILABLE");
    if (skipped > 0) { remaining -= skipped; continue; }
    int got = input.read(skip, 0, (int)Math.min(skip.length, remaining));
    if (got < 0) throw new Failure("STALE_VERSION");
    if (got == 0) throw new Failure("UNAVAILABLE"); remaining -= got;
   }
   int written = 0;
   while (written < bytes.length) {
    operation.check(); int got = input.read(bytes, written, bytes.length - written);
    if (got < 0) throw new Failure("STALE_VERSION");
    if (got == 0) throw new Failure("UNAVAILABLE"); written += got;
   }
   operation.check();
  } finally { operation.attach(null); }
  if (!before.same(resolve(tree, path, operation))) throw new Failure("STALE_VERSION");
  return frame(new JSONObject().put("type", "data").put("offset", offset), bytes);
 }

 private AssetFileDescriptor open(Uri uri, Operation operation) throws Exception {
  operation.check();
  AssetFileDescriptor descriptor = context.getContentResolver().openAssetFileDescriptor(uri, "r", operation.signal);
  if (descriptor == null) throw new IOException("Shared document unavailable");
  return descriptor;
 }
 private Row resolve(Uri tree, String path, Operation operation) throws Exception {
  Row current = metadata(tree, document(tree), operation == null ? null : operation.signal);
  for (String name : parts(path)) {
   if (!current.directory) throw new Failure("PATH_DENIED");
   Row found = null;
   Uri children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, current.id);
   try (Cursor rows = context.getContentResolver().query(children, COLUMNS, null, null, null, operation.signal)) {
    if (rows == null) throw new IOException("Shared folder unavailable"); int count = 0;
    while (rows.moveToNext()) {
     operation.check(); if (++count > MAX_SCAN) throw new Failure("RESOURCE_LIMIT");
     Row candidate = new Row(tree, rows);
     if (name.equals(candidate.name)) {
      if (found != null) throw new Failure("PATH_DENIED"); found = candidate;
     }
    }
   }
   if (found == null) throw new Failure("NOT_FOUND");
   if (found.virtual) throw new Failure("PATH_DENIED"); current = found;
  }
  return current;
 }
 private Row metadata(Uri tree, Uri uri, CancellationSignal signal) throws Exception {
  try (Cursor rows = context.getContentResolver().query(uri, COLUMNS, null, null, null, signal)) {
   if (rows == null || !rows.moveToFirst()) throw new IOException("Shared document unavailable");
   return new Row(tree, rows);
  }
 }
 private static Uri document(Uri tree) { return DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree)); }
 private static String[] parts(String path) throws Failure {
  if (path.getBytes(StandardCharsets.UTF_8).length > 4096) throw new Failure("PATH_DENIED");
  if (path.isEmpty()) return new String[0];
  String[] parts = path.split("/", -1);
  for (String part : parts) if (!validName(part)) throw new Failure("PATH_DENIED");
  return parts;
 }
 private static boolean validName(String name) {
  return !name.isEmpty() && !name.equals(".") && !name.equals("..") && !name.startsWith(".wispnet-")
      && !name.contains("/") && !name.contains("\\") && !name.contains("\0")
      && StandardCharsets.UTF_8.newEncoder().canEncode(name) && name.getBytes(StandardCharsets.UTF_8).length <= 255;
 }
 private static int compareNames(String left, String right) {
  int a = 0, b = 0;
  while (a < left.length() && b < right.length()) {
   int x = left.codePointAt(a), y = right.codePointAt(b);
   if (x != y) return Integer.compare(x, y);
   a += Character.charCount(x); b += Character.charCount(y);
  }
  return Integer.compare(left.length() - a, right.length() - b);
 }
 private static String sha256(byte[] bytes) throws Exception { return java.util.HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(bytes)); }
 private static byte[] frame(JSONObject metadata, byte[] data) throws Exception {
  byte[] json = metadata.toString().getBytes(StandardCharsets.UTF_8);
  if (json.length > MAX_METADATA || data.length > MAX_CHUNK) throw new Failure("RESOURCE_LIMIT");
  return ByteBuffer.allocate(8 + json.length + data.length).putInt(json.length).putInt(data.length).put(json).put(data).array();
 }
 private static byte[] error(String code) {
  try { return frame(new JSONObject().put("type", "error").put("code", code).put("message", "Shared folder operation failed")
      .put("expected_offset", JSONObject.NULL), new byte[0]); }
  catch (Exception impossible) { throw new IllegalStateException(impossible); }
 }
 private static final class Row {
  final String id, name, mime; final Uri uri; final long size, modified; final boolean directory, virtual;
  Row(Uri tree, Cursor cursor) throws Exception {
   id = cursor.getString(0); name = cursor.getString(1); mime = cursor.getString(2);
   size = cursor.isNull(3) ? -1 : cursor.getLong(3); modified = cursor.isNull(4) ? 0 : cursor.getLong(4);
   directory = Document.MIME_TYPE_DIR.equals(mime); virtual = !cursor.isNull(5) && (cursor.getLong(5) & Document.FLAG_VIRTUAL_DOCUMENT) != 0;
   if (id == null || name == null || mime == null || size > MAX_FILE) throw new Failure("RESOURCE_LIMIT");
   uri = DocumentsContract.buildDocumentUriUsingTree(tree, id);
  }
  boolean same(Row other) { return id.equals(other.id) && name.equals(other.name) && mime.equals(other.mime) && size == other.size && modified == other.modified; }
 }
 private static final class Version {
  final Row row; final String value; final long size;
  Version(Row row, String value, long size) { this.row = row; this.value = value; this.size = size; }
 }
 private static final class Failure extends Exception { final String code; Failure(String code) { this.code = code; } }
 private static final class Operation implements AutoCloseable {
  final CancellationSignal signal = new CancellationSignal();
  final BooleanSupplier cancelled;
  final ScheduledFuture<?> watch;
  private volatile Closeable input;
  Operation(BooleanSupplier cancelled) {
   this.cancelled = cancelled;
   watch = CANCELLATION.scheduleWithFixedDelay(() -> {
    if (cancelled.getAsBoolean()) {
     signal.cancel(); Closeable current = input;
     if (current != null) try { current.close(); } catch (IOException ignored) {}
    }
   }, 100, 100, TimeUnit.MILLISECONDS);
  }
  void check() throws Failure { if (cancelled.getAsBoolean()) throw new Failure("UNAVAILABLE"); }
  void attach(Closeable value) { input = value; }
  @Override public void close() { watch.cancel(false); input = null; }
 }
}
