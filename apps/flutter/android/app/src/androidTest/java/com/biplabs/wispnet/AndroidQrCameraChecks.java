package com.biplabs.wispnet;

import android.Manifest;
import android.app.Activity;
import android.app.Instrumentation;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.BitmapFactory;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraDevice;
import android.os.Bundle;
import android.util.Size;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/** Bounds/lifecycle fixtures; real capture is opt-in and never requests permission. */
final class AndroidQrCameraChecks {
 static void run(Instrumentation test) { run(test, false); }

 static void run(Instrumentation test, boolean liveCapture) {
  Bundle result = new Bundle();
  try {
   expectSize(new Size(1280, 720), new Size[]{new Size(4096, 3000), new Size(640, 480), new Size(1280, 720)}, "Prefer the largest small JPEG");
   expectSize(new Size(1920, 1080), new Size[]{new Size(4096, 3000), new Size(1920, 1080)}, "A 1080p-only camera must remain available");
   expectSize(new Size(1080, 1920), new Size[]{new Size(3000, 4096), new Size(1080, 1920)}, "Portrait output has the same capture bounds");
   expectSize(new Size(1600, 1200), new Size[]{new Size(1920, 1080), new Size(1600, 1200)}, "Choose the smallest larger JPEG");
   expectSize(new Size(640, 480), new Size[]{new Size(1920, 1080), new Size(640, 480)}, "Prefer a small JPEG over the fallback");
   expectSize(null, new Size[]{new Size(4096, 3000), new Size(1921, 1080)}, "Outputs beyond 1920 must remain bounded");
   expectSize(null, new Size[0], "A camera without JPEG output must remain unavailable");
   expectSize(null, null, "Missing output metadata must remain unavailable");
   failureMessages();
   int[] replies = {0};
   AndroidQrCamera camera = new AndroidQrCamera(test.getTargetContext());
   camera.frame((bytes, error) -> {
    if (bytes != null || error == null) throw new AssertionError("Capture before explicit camera start must fail");
    replies[0]++;
   });
   camera.close(); camera.close();
   if (replies[0] != 1) throw new AssertionError("Frame reply completed more than once");
   if (liveCapture) liveCapture(test);
   result.putString("camera", liveCapture
       ? "bounded JPEG selection, explicit-start lifecycle, live JPEG capture and close/reopen passed"
       : "bounded JPEG selection and explicit-start lifecycle passed; no camera opened");
   test.finish(Activity.RESULT_OK, result);
  } catch (Throwable failure) {
   result.putString("error", failure.toString()); test.finish(Activity.RESULT_CANCELED, result);
  }
 }

 private static void expectSize(Size expected, Size[] sizes, String message) {
  Size actual = AndroidQrCamera.chooseSize(sizes);
  if (expected == null ? actual != null : !expected.equals(actual))
   throw new AssertionError(message + ": expected " + expected + ", received " + actual);
 }

 private static void failureMessages() {
  String busy = AndroidQrCamera.deviceErrorMessage(CameraDevice.StateCallback.ERROR_CAMERA_IN_USE);
  String disabled = AndroidQrCamera.deviceErrorMessage(CameraDevice.StateCallback.ERROR_CAMERA_DISABLED);
  String security = AndroidQrCamera.openFailureMessage(new SecurityException());
  String generic = AndroidQrCamera.openFailureMessage(new Exception());
  if (!busy.contains("in use") || !disabled.contains("turned off") || !security.contains("Allow camera access"))
   throw new AssertionError("Camera errors must identify busy, disabled and permission failures");
  if (busy.equals(disabled) || disabled.equals(security) || security.equals(generic) || generic.equals(busy) || generic.equals(disabled))
   throw new AssertionError("Camera failures must retain distinct actionable reasons");
  if (generic.contains("access") || generic.contains("permission"))
   throw new AssertionError("A generic camera failure must not imply a permission problem");
  if (!busy.equals(AndroidQrCamera.deviceErrorMessage(CameraDevice.StateCallback.ERROR_MAX_CAMERAS_IN_USE))
      || !busy.equals(AndroidQrCamera.openFailureMessage(new CameraAccessException(CameraAccessException.CAMERA_IN_USE)))
      || !busy.equals(AndroidQrCamera.openFailureMessage(new CameraAccessException(CameraAccessException.MAX_CAMERAS_IN_USE)))
      || !disabled.equals(AndroidQrCamera.openFailureMessage(new CameraAccessException(CameraAccessException.CAMERA_DISABLED))))
   throw new AssertionError("Camera callback and open failures must preserve the same actionable reason");
 }

 private static void liveCapture(Instrumentation test) throws Exception {
  if (test.getTargetContext().checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED)
   throw new AssertionError("Pregrant CAMERA permission before opting into live capture");
  Activity activity = test.startActivitySync(new Intent(test.getTargetContext(), FlutterMainActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
  test.waitForIdleSync();
  AndroidQrCamera camera = new AndroidQrCamera(test.getTargetContext());
  try {
   for (int attempt = 0; attempt < 2; attempt++) {
    CameraReply started = new CameraReply();
    camera.start(started);
    started.await("Camera start", 12);
    CameraReply frame = new CameraReply();
    camera.frame(frame);
    frame.await("JPEG capture", 10);
    byte[] image = frame.image;
    if (image == null || image.length < 2 || image.length > 4 * 1024 * 1024 || (image[0] & 255) != 255 || (image[1] & 255) != 216)
     throw new AssertionError("Camera did not return a bounded JPEG frame");
    BitmapFactory.Options dimensions = new BitmapFactory.Options();
    dimensions.inJustDecodeBounds = true;
    BitmapFactory.decodeByteArray(image, 0, image.length, dimensions);
    if (dimensions.outWidth <= 0 || dimensions.outHeight <= 0 || dimensions.outWidth > 1920 || dimensions.outHeight > 1920)
     throw new AssertionError("Captured JPEG dimensions exceed the native capture bounds");
    camera.close();
   }
  } finally {
   camera.close();
   test.runOnMainSync(activity::finish);
  }
 }

 private static final class CameraReply implements AndroidQrCamera.Reply {
  private final CountDownLatch ready = new CountDownLatch(1);
  private volatile byte[] image;
  private volatile Exception error;
  @Override public void complete(byte[] image, Exception error) {
   this.image = image; this.error = error; ready.countDown();
  }
  void await(String operation, int seconds) throws Exception {
   if (!ready.await(seconds, TimeUnit.SECONDS)) throw new AssertionError(operation + " did not complete");
   if (error != null) throw new Exception(operation + " failed: " + error.getMessage(), error);
  }
 }
}
