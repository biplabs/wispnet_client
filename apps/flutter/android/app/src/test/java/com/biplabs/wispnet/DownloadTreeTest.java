package com.biplabs.wispnet;

import static org.junit.Assert.*;
import org.junit.Test;

public final class DownloadTreeTest {
 @Test public void selectedTreePreservesUnicodeAndEmptyDirectories() throws Exception {
  DownloadTree tree = new DownloadTree("runs/checkpoint");
  assertEquals("資料😀", tree.add(tree.root, "資料😀", tree.root + "/資料😀", true, 0).relative);
  assertEquals("資料😀/weights.bin", tree.add(tree.root + "/資料😀", "weights.bin",
      tree.root + "/資料😀/weights.bin", false, 12).relative);
  tree.add(tree.root, "empty", tree.root + "/empty", true, 0);
  assertEquals(3, tree.entries.size()); assertEquals(12, tree.size);
 }
 @Test public void fullShareRootIsValidButUnrelatedParentsAreRejected() throws Exception {
  DownloadTree tree = new DownloadTree("");
  assertEquals("weights.bin", tree.add("", "weights.bin", "weights.bin", false, 0).relative);
  assertThrows(Exception.class, () -> tree.add("other", "x", "other/x", false, 1));
 }
 @Test public void untrustedEntriesCannotEscapeOrAliasTheSelectedTree() throws Exception {
  DownloadTree tree = new DownloadTree("run");
  for (String child : new String[]{"run-other/model", "run/../model", "/run/model", "run/model/x", "run/model\\x"})
   assertThrows(Exception.class, () -> tree.add("run", "model", child, false, 1));
  tree.add("run", "model", "run/model", false, 1);
  assertThrows(Exception.class, () -> tree.add("run", "model", "run/model", false, 1));
  assertThrows(Exception.class, () -> tree.add("run/model", "x", "run/model/x", false, 1));
 }
 @Test public void metadataAndAggregateSizeAreBounded() throws Exception {
  DownloadTree tree = new DownloadTree("");
  tree.add("", "large", "large", false, DownloadTree.MAX_BYTES);
  assertThrows(Exception.class, () -> tree.add("", "more", "more", false, 1));
  assertThrows(Exception.class, () -> tree.add("", "negative", "negative", false, -1));
  assertThrows(Exception.class, () -> tree.add("", "dir", "dir", true, 1));
  assertThrows(Exception.class, () -> DownloadTree.path("a/".repeat(64) + "a", false));
  assertThrows(Exception.class, () -> DownloadTree.path(("x".repeat(255) + "/").repeat(16) + "x", false));
 }
 @Test public void enumerationCannotGrowWithoutBound() throws Exception {
  DownloadTree tree = new DownloadTree("");
  for (int index = 0; index < DownloadTree.MAX_ENTRIES; index++) {
   String name = "file-" + index; tree.add("", name, name, false, 0);
  }
  assertThrows(Exception.class, () -> tree.add("", "extra", "extra", false, 0));
 }
 @Test public void cursorOrderingUsesWireUtf8Order() {
  assertTrue(DownloadTree.compareNames("\ue000", "😀") < 0);
  assertTrue(DownloadTree.compareNames("file9", "file10") > 0);
 }
}
