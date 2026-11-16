// Inspect only the owned GPU capture. No display-color conversion is performed.
import AppKit
import Foundation
import ImageIO

let args = CommandLine.arguments
guard args.count == 8,
      let file = try? FileHandle(forReadingFrom: URL(fileURLWithPath: args[1])),
      let data = try? file.read(upToCount: 32 * 1024 * 1024 + 1),
      data.count <= 32 * 1024 * 1024,
      data.starts(with: [137, 80, 78, 71, 13, 10, 26, 10]),
      let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary),
      let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
      let encodedWidth = properties[kCGImagePropertyPixelWidth] as? Int,
      let encodedHeight = properties[kCGImagePropertyPixelHeight] as? Int,
      let encodedDepth = properties[kCGImagePropertyDepth] as? Int, encodedDepth == 8,
      encodedWidth > 0, encodedWidth <= 8192, encodedHeight > 0, encodedHeight <= 8192,
      encodedWidth <= 256 * 1024 * 1024 / 4 / encodedHeight,
      let bitmap = NSBitmapImageRep(data: data), let bytes = bitmap.bitmapData,
      let width = Double(args[2]), let height = Double(args[3]), let scale = Double(args[4]),
      width.isFinite, height.isFinite, scale.isFinite, width > 1024, height > 640,
      width <= 8192, height <= 8192, width * scale <= 8192, height * scale <= 8192,
      scale >= 0.125, scale <= 16,
      !bitmap.isPlanar, bitmap.bitsPerSample == 8,
      [3, 4].contains(bitmap.samplesPerPixel),
      bitmap.bitsPerPixel == bitmap.samplesPerPixel * 8,
      bitmap.pixelsWide > 0, bitmap.pixelsWide <= 8192,
      bitmap.pixelsHigh > 0, bitmap.pixelsHigh <= 8192,
      bitmap.pixelsWide == encodedWidth, bitmap.pixelsHigh == encodedHeight,
      bitmap.bytesPerRow >= bitmap.pixelsWide * bitmap.samplesPerPixel,
      bitmap.bytesPerRow <= 256 * 1024 * 1024 / bitmap.pixelsHigh,
      bitmap.bytesPerPlane >= bitmap.bytesPerRow * bitmap.pixelsHigh,
      Int((width + 56) * scale) == bitmap.pixelsWide,
      Int(height * scale) == bitmap.pixelsHigh else {
    fputs("invalid bounded winding capture or geometry\n", stderr)
    exit(2)
}
try file.close()
let peer = [Int(args[5])!, Int(args[6])!, Int(args[7])!]
let channels = bitmap.samplesPerPixel
let green = [32, 176, 64]
let red = [192, 32, 32]
let patch = [96, 64, 208]
func raw(_ x: Int, _ y: Int, _ indices: [Int]) -> [Int] {
    let offset = y * bitmap.bytesPerRow + x * channels
    return indices.map { Int(bytes[offset + $0]) }
}
func same(_ a: [Int], _ b: [Int]) -> Bool {
    return zip(a, b).allSatisfy { abs($0 - $1) <= 1 }
}
// Derive the channel layout from two distinct known probes and verify it once.
// Row coordinates are the same coordinates used by NSBitmapImageRep.colorAt.
var candidates: [[Int]] = []
for r in 0..<channels {
    for g in 0..<channels where g != r {
        for b in 0..<channels where b != r && b != g {
            let indices = [r, g, b]
            if same(raw(Int(512 * scale), Int(512 * scale), indices), green)
                && same(raw(bitmap.pixelsWide - Int(28 * scale), Int(28 * scale), indices), peer) {
                candidates.append(indices)
            }
        }
    }
}
guard candidates.count == 1 else {
    fputs("capture channel layout did not match known probes\n", stderr)
    exit(2)
}
let indices = candidates[0]
let radius = 384 * scale
let band = 2 + 0.001 * radius
let inner = (radius - band) * (radius - band)
let outer = (radius + band) * (radius + band)
var classified = 0
var excluded = 0
var wrong = 0
var first: [String: Any]? = nil
for y in Int(64 * scale)..<Int(height * scale) {
    for x in 0..<Int(width * scale) {
        let lx = (Double(x) + 0.5) / scale
        let ly = (Double(y) + 0.5) / scale
        let dx = Double(x) + 0.5 - 512 * scale
        let dy = Double(y) + 0.5 - 512 * scale
        let distance = dx * dx + dy * dy
        let expected: [Int]
        if lx >= 634 && lx < 636 && ly >= 148 && ly < 150 {
            expected = patch
        } else if distance < inner {
            expected = green
        } else if distance > outer {
            expected = red
        } else {
            excluded += 1
            continue
        }
        classified += 1
        let actual = raw(x, y, indices)
        if !same(actual, expected) {
            wrong += 1
            if first == nil { first = ["x": x, "y": y, "actual": actual, "expected": expected] }
        }
    }
}
guard classified * 100 >= (classified + excluded) * 98 else { exit(2) }
let report: [String: Any] = ["classified": classified, "excluded_edge_pixels": excluded,
    "mismatched_pixels": wrong, "first_mismatch": first.map { $0 as Any } ?? NSNull(),
    "width": bitmap.pixelsWide, "height": bitmap.pixelsHigh, "scale": scale,
    "channel_indices": indices, "top_prefix_logical_pixels": 64, "rgb_tolerance": 1]
print(String(data: try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]), encoding: .utf8)!)
exit(wrong == 0 ? 0 : 1)
