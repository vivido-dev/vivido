// Sample the capture's encoded RGB components without a display color conversion.
import AppKit
import Foundation

let args = CommandLine.arguments
if args[1] == "--preflight" {
    guard let session = CGSessionCopyCurrentDictionary() as? [String: Any] else {
        fatalError("macOS session state is unavailable")
    }
    let flags = [
        "locked": session["CGSSessionScreenIsLocked"] as? Bool ?? false,
        "screen_capture_allowed": CGPreflightScreenCaptureAccess(),
    ]
    print(String(data: try JSONSerialization.data(withJSONObject: flags), encoding: .utf8)!)
} else if args[1] == "--window" {
    let pid = Int(args[2])!
    let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
        as? [[String: Any]] ?? []
    let owned = windows.filter {
        ($0[kCGWindowOwnerPID as String] as? Int) == pid && ($0[kCGWindowLayer as String] as? Int) == 0
    }.map { window in
        ["id": window[kCGWindowNumber as String]!, "bounds": window[kCGWindowBounds as String]!,
         "onscreen": window[kCGWindowIsOnscreen as String] ?? false] as [String: Any]
    }
    print(String(data: try JSONSerialization.data(withJSONObject: owned, options: [.sortedKeys]), encoding: .utf8)!)
} else {
    let data = try Data(contentsOf: URL(fileURLWithPath: args[1]))
    guard let bitmap = NSBitmapImageRep(data: data) else { fatalError("invalid capture") }
    var samples: [[Int]] = []
    for index in stride(from: 2, to: args.count, by: 2) {
        let x = Int(args[index])!
        let y = Int(args[index + 1])!
        guard x >= 0, x < bitmap.pixelsWide, y >= 0, y < bitmap.pixelsHigh,
              let color = bitmap.colorAt(x: x, y: y) else {
            fatalError("capture sample is outside its image")
        }
        samples.append([color.redComponent, color.greenComponent, color.blueComponent]
            .map { Int(($0 * 255).rounded()) })
    }
    print(String(data: try JSONSerialization.data(withJSONObject: samples), encoding: .utf8)!)
}
