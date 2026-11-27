package com.biplabs.wispnet;

import static org.junit.Assert.assertEquals;
import org.junit.Test;

public class AccessStatusTest {
 @Test public void matchesDesktopStatusSummary() {
  assertEquals("2 peers online · 1 active · 3 requests · Relay: connected",
      new AccessStatus(2, 1, 3, "connected").summary());
  assertEquals("0 peers online · 0 active · 0 requests · Relay: offline",
      AccessStatus.OFFLINE.summary());
 }
}
