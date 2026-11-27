package com.biplabs.wispnet;

import java.util.Iterator;
import org.json.JSONArray;
import org.json.JSONObject;

/** The same peer, activity, request and relay summary shown in the desktop tray. */
record AccessStatus(int onlinePeers, int activeClients, int pendingRequests, String relay) {
 static final AccessStatus OFFLINE = new AccessStatus(0, 0, 0, "offline");

 static AccessStatus from(JSONObject owned, JSONObject hosts) throws Exception {
  JSONObject host = owned.getJSONObject("host");
  String own = host.getString("host");
  int onlinePeers = 0;
  for (Iterator<String> keys = hosts.keys(); keys.hasNext();) {
   String peer = keys.next();
   if (!own.equals(peer) && "available".equals(hosts.getJSONObject(peer).optString("state"))) onlinePeers++;
  }

  int activeClients = 0;
  JSONObject connected = owned.getJSONObject("connected_peers");
  JSONObject recent = owned.getJSONObject("recent_peers");
  JSONArray clients = host.getJSONArray("clients");
  for (int index = 0; index < clients.length(); index++) {
   String peer = clients.getJSONObject(index).getString("peer");
   if (connected.optLong(peer) > 0 || recent.has(peer)) activeClients++;
  }

  int pendingRequests = 0;
  JSONArray invitations = owned.getJSONArray("invitations");
  for (int index = 0; index < invitations.length(); index++) {
   if ("pending".equals(invitations.getJSONObject(index).optString("state"))) pendingRequests++;
  }
  return new AccessStatus(onlinePeers, activeClients, pendingRequests, owned.optString("relay", "offline"));
 }

 String summary() {
  return onlinePeers + " peers online · " + activeClients + " active · " + pendingRequests + " requests · Relay: " + relay;
 }
}
