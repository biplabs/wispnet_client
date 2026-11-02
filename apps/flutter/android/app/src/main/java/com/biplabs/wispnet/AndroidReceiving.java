package com.biplabs.wispnet;

import android.content.Context;
import android.content.SharedPreferences;
import android.database.Cursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract;
import android.provider.DocumentsContract.Document;
import android.system.Os;
import android.system.OsConstants;
import android.util.AtomicFile;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.UUID;
import org.json.JSONArray;
import org.json.JSONObject;

/** Owner-only SAF destination configuration and verified native upload publication.
 * URI grants, staging roots and publication journals never enter peer catalogs.
 */
final class AndroidReceiving {
 private static final Object PUBLISH_LOCK = new Object();
 private final Context context;
 private final SharedPreferences roots;
 AndroidReceiving(Context context) {
  this.context = context.getApplicationContext();
  roots = context.getSharedPreferences("receiving-folders", 0);
 }

 JSONObject prepare(Uri tree) throws Exception {
  if (!"content".equals(tree.getScheme()) || !DocumentsContract.isTreeUri(tree))
   throw new IOException("Choose a receiving folder using Android's folder picker.");
  Uri directory = document(tree);
  String label;
  try (Cursor cursor = context.getContentResolver().query(directory,
      new String[]{Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE, Document.COLUMN_FLAGS}, null, null, null)) {
   if (cursor == null || !cursor.moveToFirst() || !Document.MIME_TYPE_DIR.equals(cursor.getString(1))
       || (cursor.getLong(2) & Document.FLAG_DIR_SUPPORTS_CREATE) == 0)
    throw new IOException("Choose a folder where Wispnet can create files.");
   label = cursor.getString(0);
  }
  boolean granted = false;
  for (android.content.UriPermission permission : context.getContentResolver().getPersistedUriPermissions())
   if (tree.equals(permission.getUri()) && permission.isReadPermission() && permission.isWritePermission()) granted = true;
  if (!granted) throw new SecurityException("Folder access expired. Choose the folder again.");
  // Reuse a destination's private root so selecting it again preserves resumes.
  for (String root : roots.getAll().keySet()) {
   JSONObject entry = new JSONObject(roots.getString(root, "{}"));
   if (tree.toString().equals(entry.optString("uri")))
    return new JSONObject().put("folder", root).put("label", label);
  }
  File directoryRoot = new File(context.getFilesDir(), "receiving/" + UUID.randomUUID().toString());
  if (!directoryRoot.mkdirs()) throw new IOException("Cannot prepare receiving storage.");
  Os.chmod(directoryRoot.getAbsolutePath(), 0700);
  String root = directoryRoot.getCanonicalPath();
  if (!roots.edit().putString(root, new JSONObject().put("uri", tree.toString()).put("label", label).toString()).commit())
   throw new IOException("Cannot save receiving folder.");
  return new JSONObject().put("folder", root).put("label", label);
 }

 JSONObject status() throws Exception {
  JSONObject nativeStatus = Native.command("local_files_status");
  JSONObject exports = nativeStatus.getJSONObject("exports");
  String root = exports.optString("incoming", "");
  JSONObject entry = new JSONObject(roots.getString(root, "{}"));
  boolean available = false;
  String uri = entry.optString("uri", "");
  for (android.content.UriPermission permission : context.getContentResolver().getPersistedUriPermissions())
   if (uri.equals(permission.getUri().toString()) && permission.isReadPermission() && permission.isWritePermission()) available = true;
  return new JSONObject().put("revision", nativeStatus.getLong("revision"))
      .put("clients", nativeStatus.getJSONArray("clients")).put("allow", nativeStatus.getJSONArray("allow"))
      .put("folder", root).put("label", entry.optString("label", ""))
      .put("available", available).put("online", Native.receivingOnline);
 }

 JSONObject configure(JSONObject args) throws Exception {
  JSONArray allow = args.getJSONArray("allow");
  JSONObject exports = args.optJSONObject("exports");
  if (exports == null) {
   String root = args.getString("folder");
   exports = root.isEmpty() ? new JSONObject() : new JSONObject().put("incoming", root);
  }
  // Keep all configured roots on a grant-only save. A newly selected folder
  // replaces them explicitly in the form; root markers and URI grants survive.
  for (java.util.Iterator<String> names=exports.keys();names.hasNext();)
   validateInboxRoot(exports.getString(names.next()));
  return Native.request(new JSONObject().put("op", "local_files_configure")
      .put("expected_revision", args.getLong("expected_revision"))
      .put("exports", exports)
      .put("allow", allow));
 }

 /** Each named receiving folder has an independent native service grant. */
 JSONObject inboxesStatus() throws Exception {
  JSONObject status = Native.command("local_inboxes_status");
  JSONArray rows = new JSONArray(), services = status.getJSONArray("inboxes");
  JSONObject configured = status.getJSONObject("roots");
  for (int index=0;index<services.length();index++) {
   JSONObject service = services.getJSONObject(index);
   rows.put(describeInbox(service, configured.getString(service.getString("id"))));
  }
  // Compatibility configuration created by older clients stays editable until
  // removed. A never-configured placeholder is not a folder service in the app.
  JSONObject legacy = status.getJSONObject("legacy_exports");
  if (legacy.length()>0) {
   String root = legacy.optString("incoming", "");
   if (root.isEmpty()) root = legacy.getString(legacy.keys().next());
   rows.put(describeInbox(status.getJSONObject("legacy_service"),root));
  }
  return new JSONObject().put("revision",status.getLong("revision"))
      .put("clients",status.getJSONArray("clients")).put("inboxes",rows).put("file_exports",legacy);
 }
 private JSONObject describeInbox(JSONObject service,String root) throws Exception {
  JSONObject row = new JSONObject(service.toString());
  JSONObject entry = new JSONObject(roots.getString(root,"{}"));
  boolean available = false;
  try { validateInboxRoot(root); available = true; } catch (Exception unavailable) { /* request reselection */ }
  row.remove("backend");
  return row.put("folder",root).put("label",entry.optString("label","Receiving folder")).put("available",available);
 }
 private void validateInboxRoot(String root) throws Exception {
  if (!roots.contains(root)) throw new SecurityException("Choose a receiving folder first.");
  prepare(Uri.parse(new JSONObject(roots.getString(root,"{}")).getString("uri")));
 }
 JSONObject configureInbox(JSONObject args) throws Exception {
  if ("files".equals(args.getString("id"))) {
   if (args.has("new_id") && !"files".equals(args.getString("new_id")))
    throw new SecurityException("The built-in Files service cannot be renamed.");
   return configure(args);
  }
  String root=args.getString("folder");validateInboxRoot(root);
  JSONObject request = new JSONObject().put("op","local_inbox_configure")
      .put("expected_revision",args.getLong("expected_revision")).put("id",args.getString("id"))
      .put("root",root).put("allow",args.getJSONArray("allow"));
  if (args.has("new_id")) request.put("new_id",args.getString("new_id"));
  return Native.request(request);
 }
 JSONObject removeInbox(JSONObject args) throws Exception {
  String id=args.getString("id");
  if ("files".equals(id)) return configure(new JSONObject().put("folder","").put("allow",new JSONArray())
      .put("expected_revision",args.getLong("expected_revision")));
  return Native.request(new JSONObject().put("op","local_inbox_remove").put("id",id)
      .put("expected_revision",args.getLong("expected_revision")));
 }
 void requireInboxAvailable(String id) throws Exception {
  JSONArray rows=inboxesStatus().getJSONArray("inboxes");
  for (int index=0;index<rows.length();index++) {
   JSONObject row=rows.getJSONObject(index);
   if (id.equals(row.getString("id"))) {
    if (!row.getBoolean("available")) throw new SecurityException("Folder access expired. Choose the receiving folder again.");
    return;
   }
  }
  throw new SecurityException("Receiving inbox unavailable.");
 }

 String publish(JSONObject args, int descriptor) throws Exception {
  String id = args.getString("transfer");
  return publish(args, descriptor, () -> Native.publicationCancelled(id));
 }

 /** A folder download owns its top-level directory. Never merge a new job into
  * an unrelated pre-existing provider directory. The receipt survives retries. */
 void claimDownloadDirectory(String root, String name, String id,
     java.util.function.BooleanSupplier cancelled) throws Exception {
  synchronized (PUBLISH_LOCK) {
   FileTransferRules.filename(name);
   if (!id.matches("[0-9a-f]{64}") || !roots.contains(root)) throw new SecurityException("Unknown download folder");
   check(id, cancelled);
   Uri tree = Uri.parse(new JSONObject(roots.getString(root, "{}")).getString("uri"));
   File directory = new File(context.getFilesDir(), "download-directories");
   if (!directory.isDirectory() && !directory.mkdir()) throw new IOException("Cannot save download folder receipt");
   AtomicFile journal = new AtomicFile(new File(directory, id + ".json"));
   JSONObject record = null;
   if (journal.getBaseFile().exists()) try (FileInputStream input = journal.openRead()) {
    record = new JSONObject(new String(input.readAllBytes(), StandardCharsets.UTF_8));
    if (!root.equals(record.getString("root")) || !name.equals(record.getString("name")))
     throw new SecurityException("Download destination changed");
   }
   Uri parent = document(tree), existing = find(tree, parent, name);
   if (existing != null && (record == null || !existing.toString().equals(record.getString("uri"))))
    throw new Native.Failure("A folder with this name already exists in the download folder", "CONFLICT");
   Uri output = childDirectory(tree, parent, name);
   save(journal, new JSONObject().put("root", root).put("name", name).put("uri", output.toString()));
   check(id, cancelled);
  }
 }

 /** Preserve empty directories as well as the parents created by publish(). */
 void downloadDirectory(String root, String path, java.util.function.BooleanSupplier cancelled) throws Exception {
  synchronized (PUBLISH_LOCK) {
   DownloadTree.path(path, false);
   if (!roots.contains(root)) throw new SecurityException("Unknown download folder");
   Uri tree = Uri.parse(new JSONObject(roots.getString(root, "{}")).getString("uri"));
   Uri parent = document(tree);
   for (String name : path.split("/")) { check("", cancelled); parent = childDirectory(tree, parent, name); }
  }
 }

 /** Completed publication receipts let a resumed folder skip its files without
  * trusting a name alone. Recheck actual provider bytes before reporting them. */
 boolean downloadPublished(JSONObject args, java.util.function.BooleanSupplier cancelled) throws Exception {
  synchronized (PUBLISH_LOCK) {
   String id = args.getString("transfer");
   if (!id.matches("[0-9a-f]{64}")) throw new SecurityException("Invalid download receipt");
   AtomicFile journal = new AtomicFile(new File(context.getFilesDir(), "receiving-journal/" + id + ".json"));
   if (!journal.getBaseFile().exists()) return false;
   JSONObject record;
   try (FileInputStream input = journal.openRead()) {
    record = new JSONObject(new String(input.readAllBytes(), StandardCharsets.UTF_8));
   }
   if (!args.getString("root").equals(record.getString("root")) || !args.getString("path").equals(record.getString("path"))
       || !args.getString("sha256").equals(record.getString("sha256")))
    throw new SecurityException("Download publication changed");
   if (!record.optBoolean("complete")) return false;
   if (!roots.contains(args.getString("root"))) throw new SecurityException("Unknown download folder");
   Uri tree = Uri.parse(new JSONObject(roots.getString(args.getString("root"), "{}")).getString("uri"));
   Uri expected = document(tree);
   DownloadTree.path(args.getString("path"), false);
   for (String component : args.getString("path").split("/")) {
    check(id, cancelled); expected = find(tree, expected, component);
    if (expected == null) { journal.delete(); return false; }
   }
   if (!expected.toString().equals(record.getString("uri")))
    throw new Native.Failure("The download destination now contains another file", "CONFLICT");
   java.security.MessageDigest digest = java.security.MessageDigest.getInstance("SHA-256");
   long size = 0;
   try (java.io.InputStream input = context.getContentResolver().openInputStream(Uri.parse(record.getString("uri")))) {
    if (input == null) throw new java.io.FileNotFoundException("Downloaded document unavailable");
    byte[] buffer = new byte[256 * 1024]; int count;
    while ((count = input.read(buffer)) != -1) {
     check(id, cancelled); size += count;
     if (size > args.getLong("size_bytes")) break;
     digest.update(buffer, 0, count);
    }
   } catch (java.io.FileNotFoundException missing) { journal.delete(); return false; }
   if (size != args.getLong("size_bytes") || !java.util.HexFormat.of().formatHex(digest.digest()).equals(args.getString("sha256")))
    throw new Native.Failure("A completed download was modified in the destination folder", "CONFLICT");
   return true;
  }
 }

 String publish(JSONObject args, int descriptor, java.util.function.BooleanSupplier cancelled) throws Exception {
  synchronized (PUBLISH_LOCK) {
   String id = args.getString("transfer"), root = args.getString("root"), path = args.getString("path");
   if (!id.matches("[0-9a-f]{64}") || !roots.contains(root)) throw new SecurityException("Unknown receiving folder");
   String[] components = path.split("/", -1);
   for (String name : components)
    if (name.isEmpty() || name.equals(".") || name.equals("..") || name.contains("\\") || name.startsWith(".wispnet-"))
     throw new SecurityException("Invalid filename");
   JSONObject destination = new JSONObject(roots.getString(root, "{}"));
   Uri tree = Uri.parse(destination.getString("uri"));
   File journalDirectory = new File(context.getFilesDir(), "receiving-journal");
   if (!journalDirectory.isDirectory() && !journalDirectory.mkdir()) throw new IOException("Cannot save receiving journal");
   AtomicFile journal = new AtomicFile(new File(journalDirectory, id + ".json"));
   JSONObject record = null;
   if (journal.getBaseFile().exists()) {
    try (FileInputStream input = journal.openRead()) {
     record = new JSONObject(new String(input.readAllBytes(), StandardCharsets.UTF_8));
    }
    if (!root.equals(record.getString("root")) || !path.equals(record.getString("path"))
        || !args.getString("sha256").equals(record.getString("sha256"))) throw new SecurityException("Publication changed");
    // A lost native commit response must never create a second provider file.
    if (record.optBoolean("complete")) return "ok";
   }
   check(id, cancelled);
   Uri output = record == null ? null : Uri.parse(record.getString("uri"));
   try {
    if (output == null) {
     Uri parent = document(tree);
     for (int i = 0; i < components.length - 1; i++) parent = childDirectory(tree, parent, components[i]);
     String name = components[components.length - 1];
     if (find(tree, parent, name) != null) return "conflict";
     output = DocumentsContract.createDocument(context.getContentResolver(), parent, "application/octet-stream", name);
     if (output == null) throw new IOException("Cannot create received file");
     // Providers may resolve name races by adding a suffix. Preserve the wire
     // no-replace contract rather than silently accepting another filename.
     try (Cursor cursor = context.getContentResolver().query(output, new String[]{Document.COLUMN_DISPLAY_NAME}, null, null, null)) {
      if (cursor == null || !cursor.moveToFirst() || !name.equals(cursor.getString(0))) {
       DocumentsContract.deleteDocument(context.getContentResolver(), output); return "conflict";
      }
     }
     record = new JSONObject().put("root", root).put("path", path).put("sha256", args.getString("sha256"))
         .put("uri", output.toString()).put("complete", false);
     save(journal, record);
    }
    // Native retains ownership of descriptor. fromFd duplicates it; both copies
    // are closed on every path, including a provider failure or cancellation.
    try (ParcelFileDescriptor source = ParcelFileDescriptor.fromFd(descriptor);
         FileInputStream input = new ParcelFileDescriptor.AutoCloseInputStream(source);
         ParcelFileDescriptor target = context.getContentResolver().openFileDescriptor(output, "rwt")) {
     if (target == null) throw new IOException("Cannot open receiving folder");
     try (FileOutputStream stream = new ParcelFileDescriptor.AutoCloseOutputStream(target)) {
      byte[] buffer = new byte[256 * 1024]; long written = 0; int count;
      while ((count = input.read(buffer)) != -1) { check(id, cancelled); stream.write(buffer, 0, count); written += count; }
      check(id, cancelled);
      if (written != args.getLong("size_bytes")) throw new IOException("Incomplete received file");
      stream.flush();
      if (OsConstants.S_ISREG(Os.fstat(target.getFileDescriptor()).st_mode)) stream.getFD().sync();
     }
    }
    check(id, cancelled);
    save(journal, record.put("complete", true));
    return "ok";
   } catch (Exception failure) {
    // Only a URI created and journaled by this transfer can be removed. Never
    // touch a pre-existing destination, even after a name collision.
    if (output != null) {
     try { if (DocumentsContract.deleteDocument(context.getContentResolver(), output)) journal.delete(); }
     catch (Exception unavailable) { /* retain ownership metadata for retry */ }
    }
    throw failure;
   }
  }
 }
 /** Bound publication receipts to twice native retention; discard only our own
  * abandoned provider documents, never a successfully received file. */
 void cleanup() {
  synchronized (PUBLISH_LOCK) {
   File directory = new File(context.getFilesDir(), "receiving-journal");
   File[] records = directory.listFiles();
   if (records == null) return;
   long cutoff = System.currentTimeMillis() - 48L * 60 * 60 * 1000;
   for (File file : records) {
    if (!file.getName().matches("[0-9a-f]{64}\\.json") || file.lastModified() >= cutoff) continue;
    AtomicFile journal = new AtomicFile(file);
    try (FileInputStream input = journal.openRead()) {
     JSONObject record = new JSONObject(new String(input.readAllBytes(), StandardCharsets.UTF_8));
     if (record.optBoolean("complete") || DocumentsContract.deleteDocument(context.getContentResolver(), Uri.parse(record.getString("uri")))) journal.delete();
    } catch (Exception unavailable) { /* keep ownership metadata until safe cleanup is possible */ }
   }
  }
 }
 private void check(String id, java.util.function.BooleanSupplier cancelled) throws IOException {
  if (cancelled.getAsBoolean() || !Native.receivingOnline) throw new IOException("Receiving interrupted. Retry when online.");
 }
 private static void save(AtomicFile file, JSONObject value) throws IOException {
  FileOutputStream output = file.startWrite();
  try { output.write(value.toString().getBytes(StandardCharsets.UTF_8)); file.finishWrite(output); }
  catch (IOException failure) { file.failWrite(output); throw failure; }
 }
 private static Uri document(Uri tree) { return DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree)); }
 private Uri find(Uri tree, Uri parent, String name) throws Exception {
  Uri children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, DocumentsContract.getDocumentId(parent));
  try (Cursor cursor = context.getContentResolver().query(children,
      new String[]{Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME}, null, null, null)) {
   if (cursor == null) throw new IOException("Receiving folder unavailable");
   while (cursor.moveToNext()) if (name.equals(cursor.getString(1)))
    return DocumentsContract.buildDocumentUriUsingTree(tree, cursor.getString(0));
  }
  return null;
 }
 private Uri childDirectory(Uri tree, Uri parent, String name) throws Exception {
  Uri child = find(tree, parent, name);
  boolean created = child == null;
  if (created) child = DocumentsContract.createDocument(context.getContentResolver(), parent, Document.MIME_TYPE_DIR, name);
  if (child == null) throw new IOException("Cannot create receiving subfolder");
  try (Cursor cursor = context.getContentResolver().query(child, new String[]{Document.COLUMN_MIME_TYPE, Document.COLUMN_DISPLAY_NAME}, null, null, null)) {
   if (cursor == null || !cursor.moveToFirst() || !Document.MIME_TYPE_DIR.equals(cursor.getString(0)))
    throw new IOException("Receiving path is not a folder");
   if (!name.equals(cursor.getString(1))) {
    if (created) DocumentsContract.deleteDocument(context.getContentResolver(), child);
    throw new IOException("Receiving subfolder name changed");
   }
  }
  return child;
 }
}
