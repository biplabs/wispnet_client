package com.biplabs.wispnet;

import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;

/** Validated, bounded relative names from a remote share, never local paths. */
final class DownloadTree {
 static final int MAX_ENTRIES = 10000;
 static final long MAX_BYTES = 1L << 40;
 final String root;
 final Map<String, Entry> entries = new LinkedHashMap<>();
 long size;

 static final class Entry {
  final String path, relative;
  final boolean directory;
  final long size;
  Entry(String path, String relative, boolean directory, long size) {
   this.path = path; this.relative = relative; this.directory = directory; this.size = size;
  }
 }

 DownloadTree(String root) throws Exception { this.root = path(root, true); }

 Entry add(String parent, String name, String child, boolean directory, long bytes) throws Exception {
  path(parent, true); FileTransferRules.filename(name); path(child, false);
  if (!child.equals(parent.isEmpty() ? name : parent + "/" + name))
   throw new SecurityException("The shared folder returned an invalid child path");
  if (!parent.equals(root)) {
   Entry ancestor = entries.get(parent);
   if (ancestor == null || !ancestor.directory) throw new SecurityException("The shared folder returned an unknown parent");
  }
  String prefix = root.isEmpty() ? "" : root + "/";
  if (!child.startsWith(prefix)) throw new SecurityException("The download leaves its selected folder");
  if (bytes < 0 || bytes > MAX_BYTES || (directory && bytes != 0)) throw new Exception("Invalid shared file size");
  if (entries.size() >= MAX_ENTRIES || (!directory && bytes > MAX_BYTES - size))
   throw new Exception("This folder exceeds the download limit");
  Entry entry = new Entry(child, child.substring(prefix.length()), directory, bytes);
  if (entries.putIfAbsent(child, entry) != null) throw new SecurityException("The shared folder returned a duplicate path");
  if (!directory) size += bytes;
  return entry;
 }

 static String path(String value, boolean empty) throws Exception {
  if (value == null || value.getBytes(StandardCharsets.UTF_8).length > 4096)
   throw new SecurityException("Invalid shared path");
  if (value.isEmpty() && empty) return value;
  String[] names = value.split("/", -1);
  if (names.length > 64) throw new SecurityException("This folder is nested too deeply");
  for (String name : names) FileTransferRules.filename(name);
  return value;
 }

 /** Share pagination follows UTF-8 byte order, which differs from UTF-16. */
 static int compareNames(String left, String right) {
  return java.util.Arrays.compareUnsigned(left.getBytes(StandardCharsets.UTF_8), right.getBytes(StandardCharsets.UTF_8));
 }
 private DownloadTree() { root = ""; }
}
