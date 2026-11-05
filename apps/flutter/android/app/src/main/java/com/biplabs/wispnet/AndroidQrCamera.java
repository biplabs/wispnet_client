package com.biplabs.wispnet;

import android.Manifest;
import android.content.Context;
import android.content.pm.PackageManager;
import android.graphics.ImageFormat;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCaptureSession;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraDevice;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.CaptureFailure;
import android.hardware.camera2.CaptureRequest;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.Image;
import android.media.ImageReader;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;
import android.util.Size;
import java.nio.ByteBuffer;
import java.util.Collections;

/** Native capture only. Flutter owns the QR preview, decoder and navigation. */
final class AndroidQrCamera implements AutoCloseable {
 private static final String TAG = "WispnetQrCamera";
 interface Reply { void complete(byte[] image, Exception error); }
 private final Context context;
 private HandlerThread thread;
 private Handler handler;
 private CameraDevice device;
 private CameraCaptureSession session;
 private ImageReader reader;
 private Reply starting, pendingFrame;
 private int generation;
 private int orientation;

 AndroidQrCamera(Context context) { this.context = context.getApplicationContext(); }

 synchronized void start(Reply reply) {
  if (session != null) { reply.complete(null, null); return; }
  if (starting != null) { reply.complete(null, new Exception("Camera is opening. Please wait.")); return; }
  if (context.checkSelfPermission(Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
   reply.complete(null, new SecurityException("Allow camera access or import a QR image.")); return;
  }
  starting = reply;
  final int current = ++generation;
  thread = new HandlerThread("wispnet-qr-camera"); thread.start();
  handler = new Handler(thread.getLooper());
  handler.postDelayed(() -> {
   synchronized (AndroidQrCamera.this) {
    if (current == generation && starting != null) fail("Camera opening timed out. Try again.");
   }
  }, 8000);
  handler.post(() -> open(current));
 }

 private synchronized void open(int current) {
  if (current != generation) return;
  try {
   CameraManager manager = context.getSystemService(CameraManager.class);
   String selected = null; Size size = null;
   for (String id : manager.getCameraIdList()) {
    CameraCharacteristics properties = manager.getCameraCharacteristics(id);
    StreamConfigurationMap formats = properties.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
    Size candidate = formats == null ? null : chooseSize(formats.getOutputSizes(ImageFormat.JPEG));
    if (candidate == null) continue;
    if (selected == null || Integer.valueOf(CameraCharacteristics.LENS_FACING_BACK).equals(properties.get(CameraCharacteristics.LENS_FACING))) {
     selected = id; size = candidate;
     Integer sensor = properties.get(CameraCharacteristics.SENSOR_ORIENTATION);
     orientation = sensor == null ? 0 : sensor;
     if (Integer.valueOf(CameraCharacteristics.LENS_FACING_BACK).equals(properties.get(CameraCharacteristics.LENS_FACING))) break;
    }
   }
   if (selected == null || size == null) {
    fail("No supported camera capture size. Import a QR image instead."); return;
   }
   reader = ImageReader.newInstance(size.getWidth(), size.getHeight(), ImageFormat.JPEG, 2);
   reader.setOnImageAvailableListener(source -> imageAvailable(source, current), handler);
   manager.openCamera(selected, new CameraDevice.StateCallback() {
    @Override public void onOpened(CameraDevice opened) {
     synchronized (AndroidQrCamera.this) {
      if (current != generation) { opened.close(); return; }
      device = opened;
      try {
       opened.createCaptureSession(Collections.singletonList(reader.getSurface()), new CameraCaptureSession.StateCallback() {
        @Override public void onConfigured(CameraCaptureSession configured) {
         synchronized (AndroidQrCamera.this) {
          if (current != generation) { configured.close(); return; }
          session = configured;
          Reply reply = starting; starting = null;
          if (reply != null) reply.complete(null, null);
         }
        }
        @Override public void onConfigureFailed(CameraCaptureSession failed) {
         failed.close();
         synchronized (AndroidQrCamera.this) { if (current == generation) fail("Camera configuration failed. Try again."); }
        }
       }, handler);
      } catch (Exception failure) {
       Log.w(TAG, "Cannot configure QR camera", failure);
       fail(openFailureMessage(failure));
      }
     }
    }
    @Override public void onDisconnected(CameraDevice disconnected) {
     disconnected.close();
     synchronized (AndroidQrCamera.this) { if (current == generation) fail("Camera disconnected. Try again."); }
    }
    @Override public void onError(CameraDevice failed, int error) {
     failed.close();
     synchronized (AndroidQrCamera.this) {
      if (current == generation) {
       Log.w(TAG, "QR camera error " + error);
       fail(deviceErrorMessage(error));
      }
     }
    }
   }, handler);
  } catch (Exception failure) {
   Log.w(TAG, "Cannot open QR camera", failure);
   fail(openFailureMessage(failure));
  }
 }

 static Size chooseSize(Size[] sizes) {
  Size selected = null, fallback = null;
  if (sizes == null) return null;
  for (Size size : sizes) {
   if (size.getWidth() <= 0 || size.getHeight() <= 0 || size.getWidth() > 1920 || size.getHeight() > 1920) continue;
   if (size.getWidth() <= 1280 && size.getHeight() <= 1280) {
    if (selected == null || size.getWidth() * size.getHeight() > selected.getWidth() * selected.getHeight()) selected = size;
   } else if (fallback == null || size.getWidth() * size.getHeight() < fallback.getWidth() * fallback.getHeight()) fallback = size;
  }
  // Media performance class 12+ cameras omit JPEG sizes below 1080p for
  // modern apps. Keep small captures when offered, otherwise allow 1080p.
  return selected != null ? selected : fallback;
 }

 static String deviceErrorMessage(int error) {
  switch (error) {
   case CameraDevice.StateCallback.ERROR_CAMERA_IN_USE:
   case CameraDevice.StateCallback.ERROR_MAX_CAMERAS_IN_USE:
    return "Camera is in use. Close other camera apps and try again.";
   case CameraDevice.StateCallback.ERROR_CAMERA_DISABLED:
    return "Camera access is turned off. Enable it in Android settings and try again.";
   default:
    return "Camera could not start. Try again or restart your phone.";
  }
 }

 static String openFailureMessage(Exception failure) {
  if (failure instanceof SecurityException) return "Allow camera access or import a QR image.";
  if (failure instanceof CameraAccessException) {
   switch (((CameraAccessException)failure).getReason()) {
    case CameraAccessException.CAMERA_IN_USE:
    case CameraAccessException.MAX_CAMERAS_IN_USE:
     return deviceErrorMessage(CameraDevice.StateCallback.ERROR_CAMERA_IN_USE);
    case CameraAccessException.CAMERA_DISABLED:
     return deviceErrorMessage(CameraDevice.StateCallback.ERROR_CAMERA_DISABLED);
   }
  }
  return "Camera could not start. Try again or import a QR image.";
 }

 synchronized void frame(Reply reply) {
  if (device == null || session == null || reader == null) { reply.complete(null, new Exception("Camera is not running. Try again.")); return; }
  if (pendingFrame != null) { reply.complete(null, new Exception("Camera capture is already in progress.")); return; }
  pendingFrame = reply;
  final int current = generation;
  handler.postDelayed(() -> {
   synchronized (AndroidQrCamera.this) {
    if (current == generation && pendingFrame == reply) {
     pendingFrame = null; reply.complete(null, new Exception("No camera image received. Try again."));
    }
   }
  }, 6000);
  try {
   CaptureRequest.Builder request = device.createCaptureRequest(CameraDevice.TEMPLATE_STILL_CAPTURE);
   request.addTarget(reader.getSurface());
   request.set(CaptureRequest.JPEG_QUALITY, (byte) 85);
   request.set(CaptureRequest.JPEG_ORIENTATION, orientation);
   session.capture(request.build(), new CameraCaptureSession.CaptureCallback() {
    @Override public void onCaptureFailed(CameraCaptureSession capture, CaptureRequest request, CaptureFailure failure) {
     synchronized (AndroidQrCamera.this) {
      if (current == generation && pendingFrame == reply) {
       pendingFrame = null; reply.complete(null, new Exception("Camera capture failed. Try again."));
      }
     }
    }
   }, handler);
  } catch (Exception failure) { pendingFrame = null; reply.complete(null, new Exception("Camera capture failed. Try again.")); }
 }

 private void imageAvailable(ImageReader source, int current) {
  Image image = null;
  Reply captured = null;
  try {
   image = source.acquireLatestImage();
   if (image == null) return;
   synchronized (this) {
    if (current != generation || pendingFrame == null) return;
    captured = pendingFrame; pendingFrame = null;
   }
   ByteBuffer data = image.getPlanes()[0].getBuffer();
   if (data.remaining() == 0 || data.remaining() > 4 * 1024 * 1024) { captured.complete(null, new Exception("Camera image exceeded the size limit.")); return; }
   byte[] bytes = new byte[data.remaining()]; data.get(bytes);
   captured.complete(bytes, null);
  } catch (Exception failure) {
   if (captured != null) { captured.complete(null, new Exception("Cannot read camera image. Try again.")); return; }
   synchronized (this) {
    if (current == generation && pendingFrame != null) {
     Reply reply = pendingFrame; pendingFrame = null; reply.complete(null, new Exception("Cannot read camera image. Try again."));
    }
   }
  } finally { if (image != null) image.close(); }
 }

 private synchronized void fail(String message) {
  Reply reply = starting; starting = null;
  close();
  if (reply != null) reply.complete(null, new Exception(message));
 }

 @Override public synchronized void close() {
  generation++;
  Reply start = starting, frame = pendingFrame; starting = null; pendingFrame = null;
  if (session != null) { session.close(); session = null; }
  if (device != null) { device.close(); device = null; }
  if (reader != null) { reader.close(); reader = null; }
  if (thread != null) { thread.quitSafely(); thread = null; handler = null; }
  if (start != null) start.complete(null, new Exception("Camera closed."));
  if (frame != null) frame.complete(null, new Exception("Camera closed."));
 }
}
