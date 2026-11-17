package com.biplabs.wispnet;
import android.content.Context;
import android.content.SharedPreferences;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.util.Base64;
import java.io.File;
import java.security.KeyStore;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;
import org.json.JSONObject;

public final class Native {
 static { System.loadLibrary("wispnet_android"); }
 public static native String call(String request);
 public static native String putFileDescriptor(String request, int detachedFd);
 static final class Failure extends Exception {final String code;Failure(String message,String value){super(message);code=value;}}
 static volatile boolean ready=false;
 static JSONObject request(JSONObject request) throws Exception {
  return response(call(request.toString()));
 }
 static JSONObject requestFileDescriptor(JSONObject request, android.os.ParcelFileDescriptor descriptor) throws Exception {
  int fd=descriptor.detachFd();
  try { return response(putFileDescriptor(request.toString(),fd)); }
  catch (UnsatisfiedLinkError unavailable) {
   try { android.os.ParcelFileDescriptor.adoptFd(fd).close(); } catch (Exception ignored) {}
   throw new Exception("Update Wispnet before sending files from Android.");
  }
 }
 private static JSONObject response(String raw) throws Exception {
  if(raw==null)throw new Exception("Native client unavailable");
  JSONObject reply=new JSONObject(raw);if(!reply.getBoolean("ok"))throw new Failure(reply.optString("error","Operation failed"),reply.optString("code","operation_failed"));return reply.getJSONObject("result");
 }
 static JSONObject command(String op) throws Exception {return request(new JSONObject().put("op",op));}
 private static volatile Context receivingContext;
 static volatile boolean receivingOnline;
 static native boolean publicationCancelled(String transfer);
 static native boolean shareSourceCancelled(String operation);
 static byte[] shareSourceRequest(String request) {
  try {
   JSONObject envelope = new JSONObject(request);
   String operation = envelope.getString("cancel_id");
   if (receivingContext == null) throw new SecurityException("Shared folder unavailable");
   return new AndroidShares(receivingContext).request(envelope, () -> shareSourceCancelled(operation));
  } catch (Exception unavailable) {
   // The provider adapter normally returns typed frames. Empty replies fail
   // closed in native code if Java cannot even decode the callback envelope.
   return new byte[0];
  }
 }
 static String publishReceivedFile(String request, int descriptor) {
  try {
   if (receivingContext == null) return "denied";
   return new AndroidReceiving(receivingContext).publish(new JSONObject(request), descriptor);
  } catch (SecurityException denied) { return "denied"; }
  catch (Exception failure) { return "unavailable"; }
 }
 static synchronized void ensure(Context context) throws Exception {
  if(ready)return;
  receivingContext=context.getApplicationContext();
  SharedPreferences prefs=context.getSharedPreferences("identity",Context.MODE_PRIVATE);
  KeyStore store=KeyStore.getInstance("AndroidKeyStore");store.load(null);
  String encrypted=prefs.getString("encrypted_device_key",null);String key;
  if(encrypted==null){
   if(!store.containsAlias("wispnet.identity.v1")){
    KeyGenerator generator=KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES,"AndroidKeyStore");
    generator.init(new KeyGenParameterSpec.Builder("wispnet.identity.v1",KeyProperties.PURPOSE_ENCRYPT|KeyProperties.PURPOSE_DECRYPT).setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).build());generator.generateKey();
   }
   key=command("new_key").getString("key");
   Cipher cipher=Cipher.getInstance("AES/GCM/NoPadding");cipher.init(Cipher.ENCRYPT_MODE,(SecretKey)store.getKey("wispnet.identity.v1",null));
   String envelope=Base64.encodeToString(cipher.getIV(),Base64.NO_WRAP)+":"+Base64.encodeToString(cipher.doFinal(key.getBytes(java.nio.charset.StandardCharsets.US_ASCII)),Base64.NO_WRAP);
   if(!prefs.edit().putString("encrypted_device_key",envelope).commit())throw new Exception("Cannot protect device identity");
  }else{
   String[] envelope=encrypted.split(":",-1);if(envelope.length!=2)throw new Exception("Protected identity is corrupt");
   Cipher cipher=Cipher.getInstance("AES/GCM/NoPadding");cipher.init(Cipher.DECRYPT_MODE,(SecretKey)store.getKey("wispnet.identity.v1",null),new GCMParameterSpec(128,Base64.decode(envelope[0],Base64.NO_WRAP)));
   key=new String(cipher.doFinal(Base64.decode(envelope[1],Base64.NO_WRAP)),java.nio.charset.StandardCharsets.US_ASCII);
  }
  File state=new File(context.getFilesDir(),"client");if(!state.exists()&&!state.mkdir())throw new Exception("Cannot prepare protected client state");android.system.Os.chmod(state.getAbsolutePath(),0700);
  JSONObject init=new JSONObject().put("op","init").put("key",key).put("state",state.getAbsolutePath()).put("relay","https://relay.wispnet.app").put("backup_relays",new org.json.JSONArray().put("https://usw1-1.relay.n0.iroh.link"));
  File testRelay=new File(context.getFilesDir(),"test-relay.json");
  if((context.getApplicationInfo().flags & android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE)!=0 && testRelay.isFile()){
   JSONObject fixture=new JSONObject(new String(java.nio.file.Files.readAllBytes(testRelay.toPath()),java.nio.charset.StandardCharsets.UTF_8));
   java.net.URI relay=new java.net.URI(fixture.getString("relay"));
   if(!"https".equals(relay.getScheme()) || !"127.0.0.1".equals(relay.getHost()) || relay.getPort()<=0)throw new Exception("Invalid local test relay");
   init.put("relay",relay.toString()).put("backup_relays",new org.json.JSONArray()).put("relay_ca_file",new File(context.getFilesDir(),"test-relay.pem").getAbsolutePath());
  }
  request(init);
  request(new JSONObject().put("op","set_device_id").put("device_id",DeviceProfile.name(context)));
  ready=true;context.getSharedPreferences("app_access",0).edit().clear().apply();new HostStore(context).reconcile();
 }
 private Native(){}
}
