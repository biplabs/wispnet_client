import Flutter
import UIKit
import Security
import SafariServices
import UniformTypeIdentifiers
import Vision
import ImageIO
import AVFoundation
import Network

@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate, UIDocumentPickerDelegate {
  private let worker = DispatchQueue(label: "app.wispnet.native", qos: .userInitiated)
  private let monitor = NWPathMonitor()
  private var initialized = false
  private var pendingLink: String?
  private var pickerReply: FlutterResult?
  private var pickerKind = ""
  private let camera = QrCamera()
  private var channel: FlutterMethodChannel?
  private var folderScopes: [URL] = []

  func didInitializeImplicitFlutterEngine(_ bridge: FlutterImplicitEngineBridge) {
    GeneratedPluginRegistrant.register(with: bridge.pluginRegistry)
    channel = FlutterMethodChannel(name: "app.wispnet/ui", binaryMessenger: bridge.applicationRegistrar.messenger())
    channel?.setMethodCallHandler { [weak self] call, reply in self?.handle(call, reply) }
    monitor.pathUpdateHandler = { [weak self] _ in
      guard let self else { return }
      self.worker.async {
        if self.initialized { _ = try? self.native(["method":"networkChanged", "arguments":[:]]) }
      }
    }
    monitor.start(queue: DispatchQueue(label: "app.wispnet.network"))
  }

  private var presenter: UIViewController? {
    let root = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
      .flatMap(\.windows).first(where: \.isKeyWindow)?.rootViewController
    var current = root
    while let presented = current?.presentedViewController { current = presented }
    return current
  }

  private func encode(_ value: Any) throws -> String {
    String(decoding: try JSONSerialization.data(withJSONObject: value), as: UTF8.self)
  }

  private func native(_ request: [String: Any]) throws -> [String: Any] {
    let text = try encode(request)
    guard let pointer = text.withCString({ wispnet_ios_call($0) }) else { throw failure("Native adapter unavailable") }
    defer { wispnet_ios_free(pointer) }
    let data = Data(String(cString: pointer).utf8)
    guard let response = try JSONSerialization.jsonObject(with: data) as? [String: Any], response["ok"] as? Bool == true else {
      let response = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
      throw failure(response?["error"] as? String ?? "Native operation failed")
    }
    return response["result"] as? [String: Any] ?? [:]
  }

  private func failure(_ message: String) -> NSError { NSError(domain: "Wispnet", code: 1, userInfo: [NSLocalizedDescriptionKey: message]) }

  private func identity() throws -> String {
    let query: [String: Any] = [kSecClass as String:kSecClassGenericPassword,
      kSecAttrService as String:"app.wispnet.ios.identity", kSecAttrAccount as String:"device-key"]
    var lookup = query
    lookup[kSecReturnData as String] = true
    var item: CFTypeRef?
    let status = SecItemCopyMatching(lookup as CFDictionary, &item)
    if status == errSecSuccess, let data = item as? Data, let key = String(data:data, encoding:.utf8), key.count == 64 { return key }
    guard status == errSecItemNotFound else { throw failure("Cannot read the device identity from Keychain (\(status))") }
    var bytes = [UInt8](repeating: 0, count: 32)
    guard SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess else { throw failure("Cannot create device identity") }
    let key = bytes.map { String(format:"%02x", $0) }.joined()
    var insert = query
    insert[kSecValueData as String] = Data(key.utf8)
    insert[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
    guard SecItemAdd(insert as CFDictionary, nil) == errSecSuccess else { throw failure("Cannot store device identity") }
    return key
  }

  private func initialize() throws {
    if initialized { return }
    let bookmarks=UserDefaults.standard.dictionary(forKey:"folder-bookmarks") as? [String:Data] ?? [:]
    for (_, data) in bookmarks {
      var stale=false
      if let url=try? URL(resolvingBookmarkData:data,options:.withoutUI,bookmarkDataIsStale:&stale), !stale, url.startAccessingSecurityScopedResource() { folderScopes.append(url) }
    }
    var state = try FileManager.default.url(for:.applicationSupportDirectory, in:.userDomainMask, appropriateFor:nil, create:true).appendingPathComponent("Wispnet", isDirectory:true)
    try FileManager.default.createDirectory(at:state, withIntermediateDirectories:true, attributes:[.posixPermissions:0o700, .protectionKey:FileProtectionType.completeUntilFirstUserAuthentication])
    var resources=URLResourceValues(); resources.isExcludedFromBackup=true
    try state.setResourceValues(resources)
    _ = try native(["method":"initialize", "key":identity(), "state":state.resolvingSymlinksInPath().path])
    initialized = true
  }

  private func handle(_ call: FlutterMethodCall, _ reply: @escaping FlutterResult) {
    do {
      let args = try JSONSerialization.jsonObject(with:Data((call.arguments as? String ?? "{}").utf8)) as? [String:Any] ?? [:]
      switch call.method {
      case "takeLink":
        let link = pendingLink; pendingLink = nil
        reply(try encode(link.map { ["link":$0] } ?? [:])); return
      case "takeShare", "takeInvitation": reply("{}"); return
      case "launch":
        guard let text=args["url"] as? String, let url=URL(string:text), ["http","https"].contains(url.scheme?.lowercased() ?? ""), let presenter else { throw failure("Invalid browser URL") }
        // Keep the embedded networking process foregrounded while browsing.
        presenter.present(SFSafariViewController(url:url), animated:true)
        reply("{}"); return
      case "qrCapabilities": reply(try encode(["camera":AVCaptureDevice.default(for:.video) != nil,"import":true])); return
      case "startQrCamera": camera.start(reply); return
      case "qrCameraFrame": camera.frame(reply); return
      case "stopQrCamera": camera.stop(); reply("{}"); return
      case "importQr", "pickFiles", "pickReceivingFolder", "pickShareFolder", "pickDownloadFolder":
        guard pickerReply == nil, let presenter else { throw failure("A picker is already open") }
        pickerReply=reply; pickerKind=call.method
        let folder=call.method.hasSuffix("Folder")
        let picker=UIDocumentPickerViewController(forOpeningContentTypes:folder ? [.folder] : call.method == "importQr" ? [.image] : [.item], asCopy:!folder)
        picker.allowsMultipleSelection=call.method == "pickFiles"; picker.delegate=self
        presenter.present(picker,animated:true); return
      default: break
      }
      worker.async {
        do {
          try self.initialize()
          var result = try self.native(["method":call.method,"arguments":args])
          if call.method == "snapshot" {
            result["version"] = Bundle.main.object(forInfoDictionaryKey:"CFBundleShortVersionString") as? String ?? ""
            let path=self.monitor.currentPath
            result["network"]=path.status != .satisfied ? "Offline" : path.usesInterfaceType(.wifi) ? "Wi-Fi" : path.usesInterfaceType(.cellular) ? "Cellular" : "Network available"
          }
          if call.method == "launchPublic", let text=result["url"] as? String, let url=URL(string:text), ["http","https"].contains(url.scheme ?? "") {
            DispatchQueue.main.async { self.presenter?.present(SFSafariViewController(url:url),animated:true) }
          }
          let encoded = try self.encode(result)
          DispatchQueue.main.async { reply(encoded) }
        } catch { DispatchQueue.main.async { reply(FlutterError(code:"ios",message:error.localizedDescription,details:nil)) } }
      }
    } catch { reply(FlutterError(code:"ios",message:error.localizedDescription,details:nil)) }
  }

  func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
    let reply=pickerReply; pickerReply=nil; reply?("{}")
  }
  func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls:[URL]) {
    guard let reply=pickerReply else { return }; pickerReply=nil
    let kind=pickerKind
    worker.async {
      do {
        if kind.hasSuffix("Folder") {
          guard let url=urls.first, url.startAccessingSecurityScopedResource() else { throw self.failure("Folder access unavailable. Choose it again.") }
          let path=url.resolvingSymlinksInPath().path
          let bookmark=try url.bookmarkData(options:.minimalBookmark,includingResourceValuesForKeys:nil,relativeTo:nil)
          var bookmarks=UserDefaults.standard.dictionary(forKey:"folder-bookmarks") as? [String:Data] ?? [:]
          bookmarks[path]=bookmark; UserDefaults.standard.set(bookmarks,forKey:"folder-bookmarks")
          self.folderScopes.append(url)
          let result=try self.encode(["folder":path,"path":path,"label":url.lastPathComponent])
          DispatchQueue.main.async { reply(result) }
        } else if kind == "importQr" {
          guard let url=urls.first else { throw self.failure("No image selected") }
          guard let size=try url.resourceValues(forKeys:[.fileSizeKey]).fileSize, size<=4*1024*1024 else {throw self.failure("Choose a QR image smaller than 4 MB")}
          let data=try Data(contentsOf:url, options:.mappedIfSafe)
          guard data.count<=4*1024*1024 else { throw self.failure("Choose a QR image smaller than 4 MB") }
          guard let source=CGImageSourceCreateWithData(data as CFData,nil), CGImageSourceGetCount(source)==1,
            let properties=CGImageSourceCopyPropertiesAtIndex(source,0,nil) as? [CFString:Any],
            let width=properties[kCGImagePropertyPixelWidth] as? Int, let height=properties[kCGImagePropertyPixelHeight] as? Int,
            width>0, height>0, width<=16384, height<=16384, width*height<=16*1024*1024,
            let image=CGImageSourceCreateThumbnailAtIndex(source,0,[kCGImageSourceCreateThumbnailFromImageAlways:true,kCGImageSourceThumbnailMaxPixelSize:2048,kCGImageSourceCreateThumbnailWithTransform:true] as CFDictionary)
          else {throw self.failure("Choose a still QR image with valid dimensions")}
          let request=VNDetectBarcodesRequest(); request.symbologies=[.qr]
          try VNImageRequestHandler(cgImage:image).perform([request])
          let codes=Set((request.results ?? []).compactMap(\.payloadStringValue).filter { $0.hasPrefix("wispnet1:") && $0.utf8.count<=8192 })
          guard codes.count==1, let code=codes.first else { throw self.failure("Choose an image containing one Wispnet pairing QR") }
          let result=try self.encode(["code":code]); DispatchQueue.main.async { reply(result) }
        } else {
          let directory=FileManager.default.temporaryDirectory.appendingPathComponent("WispnetUploads",isDirectory:true)
          try FileManager.default.createDirectory(at:directory,withIntermediateDirectories:true)
          let files=try urls.map { url -> [String:Any] in
            let destination=directory.appendingPathComponent(UUID().uuidString+"-"+url.lastPathComponent)
            try FileManager.default.copyItem(at:url,to:destination)
            guard try destination.resourceValues(forKeys:[.isRegularFileKey]).isRegularFile == true else { throw self.failure("Choose regular files to send") }
            let attrs=try FileManager.default.attributesOfItem(atPath:destination.path)
            return ["path":destination.path,"name":url.lastPathComponent,"size_bytes":attrs[.size] ?? 0]
          }
          let result=try self.encode(["paths":files.compactMap { $0["path"] },"files":files])
          DispatchQueue.main.async { reply(result) }
        }
      } catch { DispatchQueue.main.async { reply(FlutterError(code:"picker",message:error.localizedDescription,details:nil)) } }
    }
  }

  func receive(_ url: URL) { if url.scheme == "wispnet", url.absoluteString.utf8.count<=8192 { pendingLink=url.absoluteString } }
}

private final class QrCamera: NSObject, AVCapturePhotoCaptureDelegate {
  private let queue=DispatchQueue(label:"app.wispnet.camera")
  private var session:AVCaptureSession?
  private var output:AVCapturePhotoOutput?
  private var pending:FlutterResult?
  private var generation=0
  func start(_ reply:@escaping FlutterResult) {
    let generation=queue.sync { self.generation += 1; return self.generation }
    AVCaptureDevice.requestAccess(for:.video) { allowed in
      guard allowed else { DispatchQueue.main.async { reply(FlutterError(code:"camera_permission",message:"Allow camera access in Settings to scan a pairing QR.",details:nil)) }; return }
      self.queue.async {
        guard generation == self.generation else {DispatchQueue.main.async {reply(FlutterError(code:"camera_cancelled",message:"Camera closed",details:nil))}; return}
        do {
          self.session?.stopRunning()
          let session=AVCaptureSession(); session.sessionPreset = .photo
          guard let device=AVCaptureDevice.default(for:.video) else { throw NSError(domain:"Camera unavailable",code:1) }
          let input=try AVCaptureDeviceInput(device:device), output=AVCapturePhotoOutput()
          guard session.canAddInput(input),session.canAddOutput(output) else { throw NSError(domain:"Camera unavailable",code:1) }
          session.addInput(input); session.addOutput(output); self.session=session; self.output=output
          session.startRunning(); DispatchQueue.main.async { reply("{}") }
        } catch { DispatchQueue.main.async { reply(FlutterError(code:"camera",message:error.localizedDescription,details:nil)) } }
      }
    }
  }
  func frame(_ reply:@escaping FlutterResult) {
    queue.async {
      guard self.pending == nil, let output=self.output else { DispatchQueue.main.async { reply(FlutterError(code:"camera",message:"Camera is unavailable or busy",details:nil)) }; return }
      self.pending=reply
      output.capturePhoto(with:AVCapturePhotoSettings(),delegate:self)
    }
  }
  func stop() { queue.async {
    self.generation += 1; self.session?.stopRunning(); self.session=nil; self.output=nil
    let reply=self.pending; self.pending=nil
    DispatchQueue.main.async {reply?(FlutterError(code:"camera_cancelled",message:"Camera closed",details:nil))}
  } }
  func photoOutput(_ output:AVCapturePhotoOutput,didFinishProcessingPhoto photo:AVCapturePhoto,error:Error?) {
    queue.async {
      guard let reply=self.pending else { return }; self.pending=nil
      guard error == nil, let data=photo.fileDataRepresentation(), let image=UIImage(data:data) else { DispatchQueue.main.async { reply(FlutterError(code:"camera",message:"Cannot capture camera image",details:nil)) }; return }
      let scale=min(1,1280/max(image.size.width,image.size.height))
      let resized=UIGraphicsImageRenderer(size:CGSize(width:image.size.width*scale,height:image.size.height*scale)).image { _ in image.draw(in:CGRect(origin:.zero,size:CGSize(width:image.size.width*scale,height:image.size.height*scale))) }
      guard let jpeg=resized.jpegData(compressionQuality:0.8), jpeg.count<=4*1024*1024 else { DispatchQueue.main.async { reply(FlutterError(code:"camera",message:"Camera image too large",details:nil)) }; return }
      let result="{\"image_b64\":\"\(jpeg.base64EncodedString())\"}"
      DispatchQueue.main.async { reply(result) }
    }
  }
}
