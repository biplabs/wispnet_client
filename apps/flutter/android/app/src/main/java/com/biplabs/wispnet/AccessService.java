package com.biplabs.wispnet;
import android.app.*;
import android.content.*;
import android.net.*;
import android.os.*;
import org.json.*;
import java.util.concurrent.*;

/** Owns browser access independently of the activity. Runs in a dedicated process; never restarts after process death. */
public final class AccessService extends Service {
 public volatile boolean enabled=false;
 public volatile String networkStatus="Access stopped";
 private final ScheduledExecutorService profileWork=Executors.newSingleThreadScheduledExecutor();
 private final ExecutorService networkWork=Executors.newSingleThreadExecutor();
 private final Object networkLock=new Object();
 private boolean destroyed=false,worker=false,starting=false,requestedOnline=false;
 private long generation=0;
 private boolean interrupted=false;
 private NetworkController controller;
 private volatile CompletableFuture<Void> online = new CompletableFuture<>();
 private final Messenger messenger = new Messenger(new Handler(Looper.getMainLooper(), message -> {
  if (message.what != 1 || message.replyTo == null || message.sendingUid != android.os.Process.myUid()) return true;
  int id = message.arg1; Messenger reply = message.replyTo; Bundle data = message.getData();
  controller.work.execute(() -> {
   Bundle payload = new Bundle();
   try {
    String raw = data.getString("arguments", "{}");
    if (raw.length() > 131072) throw new Exception("Request too large");
    String method = data.getString("method", ""); JSONObject args = new JSONObject(raw), result;
    if ("setAccess".equals(method)) {
     if (args.getBoolean("enabled")) startAccess().get(30, TimeUnit.SECONDS); else stopAccess();
     result = new JSONObject().put("enabled", enabled);
    } else { Native.ensure(this); result = controller.dispatch(method, args); }
    String encoded = result.toString();
    if (encoded.length() > 262144) throw new Exception("Response too large");
    payload.putString("result", encoded);
   } catch (Exception failure) {
    if (failure instanceof ExecutionException && failure.getCause() instanceof Exception) failure = (Exception)failure.getCause();
    payload.putString("error", failure.getMessage() == null ? "Networking operation failed" : failure.getMessage());
    payload.putString("code", failure instanceof Native.Failure ? ((Native.Failure)failure).code : "operation_failed");
   }
   Message response = Message.obtain(null, 1, id, 0); response.setData(payload);
   try { reply.send(response); } catch (RemoteException windowClosed) {}
  });
  return true;
 }));
 private Network current;
 private ConnectivityManager manager;
 private ConnectivityManager.NetworkCallback callback;
 public void onCreate(){
  super.onCreate();controller=new NetworkController(this);profileWork.scheduleWithFixedDelay(()->{if(enabled){DeviceProfile.syncAll(this);new AndroidReceiving(this).cleanup();}},5,30,TimeUnit.SECONDS);NotificationManager nm=getSystemService(NotificationManager.class);
  nm.createNotificationChannel(new NotificationChannel("access","Wispnet browser access",NotificationManager.IMPORTANCE_LOW));
  manager=getSystemService(ConnectivityManager.class);current=manager.getActiveNetwork();
  networkStatus=current==null?"No network. Reconnect, then reopen the service.":"Network available";
  callback=new ConnectivityManager.NetworkCallback(){
   public void onAvailable(Network network){synchronized(networkLock){if(destroyed||network.equals(current))return;current=network;changed();}}
   public void onLost(Network network){synchronized(networkLock){if(destroyed||!network.equals(current))return;current=null;changed();}}
  };manager.registerDefaultNetworkCallback(callback);
 }
 static String reconnectedMessage(boolean interrupted){return interrupted?"Reconnected. A page was interrupted; reload that page to continue.":"Network available";}
 private Notification notification(String title){
  Intent stop=new Intent(this,AccessService.class).setAction("stop");
  PendingIntent pending=PendingIntent.getService(this,2,stop,PendingIntent.FLAG_IMMUTABLE|PendingIntent.FLAG_UPDATE_CURRENT);
  PendingIntent open=PendingIntent.getActivity(this,1,getPackageManager().getLaunchIntentForPackage(getPackageName()),PendingIntent.FLAG_IMMUTABLE);
  return new Notification.Builder(this,"access").setSmallIcon(R.drawable.ic_stat_wispnet).setContentTitle(title).setContentText("There’s no them. There’s only us.").setContentIntent(open).addAction(new Notification.Action.Builder(null,"Go offline",pending).build()).setOngoing(true).setOnlyAlertOnce(true).setShowWhen(false).setCategory(Notification.CATEGORY_SERVICE).setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE).build();
 }
 // Call while holding networkLock. Drain every generation, including a callback
 // arriving during native recovery; coalescing must never lose the final network.
 private void changed(){
  if (!enabled || !Native.ready) { networkStatus=current==null?"No network. Waiting for a connection…":"Network available"; return; }
  generation++;networkStatus=current==null?"No network. Waiting for a connection…":"Reconnecting…";
  if(worker)return;worker=true;
  networkWork.execute(()->{while(true){long observed;synchronized(networkLock){if(destroyed){worker=false;return;}observed=generation;}
   boolean failed=false;
   try{
    int count=Native.command("network").optInt("interrupted_requests",0);
    synchronized(networkLock){interrupted|=count>0;}
   }catch(Exception ignored){failed=true;}
   synchronized(networkLock){
    if(destroyed){worker=false;return;}
    if(observed!=generation)continue;
    if(failed)networkStatus="Could not reconnect. Go offline, then online to try again.";
    else if(current!=null){
     networkStatus=reconnectedMessage(interrupted);
     if(interrupted){
      final String notice=networkStatus;
      profileWork.schedule(()->{synchronized(networkLock){if(!destroyed&&observed==generation&&notice.equals(networkStatus))networkStatus="Network available";}},10,TimeUnit.SECONDS);
     }
     interrupted=false;
    }
    worker=false;return;
   }
  }});
 }

 public int onStartCommand(Intent intent,int flags,int startId){
  if(intent!=null&&"stop".equals(intent.getAction())){controller.work.execute(this::stopAccess);return START_NOT_STICKY;}
  startAccess();return START_NOT_STICKY;
 }
 private CompletableFuture<Void> startAccess(){
  synchronized(networkLock){
   if(destroyed)return CompletableFuture.failedFuture(new IllegalStateException("Networking stopped"));
   requestedOnline=true;
   if(online.isDone()&&!enabled)online=new CompletableFuture<>();
   if(starting||enabled)return online;
   starting=true;
   startForeground(1,notification("Wispnet starting…"));
   networkWork.execute(()->{
    try{
     Native.ensure(this);
     synchronized(networkLock){
      if(destroyed||!requestedOnline)return;
      Native.command("network");
      Native.command("enable");Native.receivingOnline=true;enabled=true;controller.filesOnline();
      networkStatus=current==null?"No network. Waiting for a connection…":"Network available";
      online.complete(null);
      getSystemService(NotificationManager.class).notify(1,notification("Wispnet online"));
     }
    }catch(Exception failure){networkStatus="Cannot start browser access: "+failure.getMessage();online.completeExceptionally(failure);stopAccess();}
    finally{synchronized(networkLock){starting=false;}}
   });
   return online;
  }
 }
 void stopAccess(){
  synchronized(networkLock){ Native.receivingOnline=false;requestedOnline=false;online.completeExceptionally(new IllegalStateException("Wispnet is offline"));enabled=false;controller.cancelFiles();try{if(Native.ready)Native.command("stop");}catch(Exception ignored){} }
  stopForeground(STOP_FOREGROUND_REMOVE);stopSelf();
 }
 public void onDestroy(){
  Native.receivingOnline=false;enabled=false;synchronized(networkLock){destroyed=true;}
  try{if(callback!=null)manager.unregisterNetworkCallback(callback);}catch(Exception ignored){}
  try{Native.command("stop");}catch(Exception ignored){}
  controller.close();profileWork.shutdownNow();networkWork.shutdown();networkStatus="Access stopped. Go online to use your saved service addresses.";
  stopForeground(STOP_FOREGROUND_REMOVE);super.onDestroy();
 }
 @Override public void onTimeout(int startId,int fgsType){stopAccess();}
 public IBinder onBind(Intent intent){return messenger.getBinder();}
}
