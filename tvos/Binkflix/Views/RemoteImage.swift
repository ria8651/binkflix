import ImageIO
import SwiftUI
import UIKit

/// Loads artwork. Images are decoded at the size they're shown at, not their
/// source size: the Apple TV HD's GPU runs out of memory (black and
/// garbled regions) scaling full-size artwork down every frame. Decoded
/// images are kept in memory; the bytes in URLCache.
@MainActor
final class ImageLoader {
    static let shared = ImageLoader()

    private let cache = NSCache<NSURL, UIImage>()
    private var inflight: [URL: Task<UIImage?, Never>] = [:]
    private let session: URLSession = {
        let config = URLSessionConfiguration.default
        config.urlCache = URLCache(memoryCapacity: 32 << 20, diskCapacity: 512 << 20)
        return URLSession(configuration: config)
    }()

    init() {
        cache.totalCostLimit = 256 << 20
    }

    /// `fit` is the area the image is drawn into, in pixels.
    func cached(_ url: URL, fit: CGSize) -> UIImage? {
        cache.object(forKey: Self.key(url, fit))
    }

    func load(_ url: URL, fit: CGSize, fill: Bool) async -> UIImage? {
        let key = Self.key(url, fit)
        if let hit = cache.object(forKey: key) { return hit }
        if let task = inflight[key as URL] { return await task.value }
        let session = session
        let task = Task<UIImage?, Never> {
            guard let (data, resp) = try? await session.data(from: url),
                  (resp as? HTTPURLResponse)?.statusCode == 200 else { return nil }
            return await Task.detached { Self.downsample(data, fit: fit, fill: fill) }.value
        }
        inflight[key as URL] = task
        let image = await task.value
        inflight[key as URL] = nil
        if let image {
            let cost = Int(image.size.width * image.size.height * image.scale * image.scale * 4)
            cache.setObject(image, forKey: key, cost: cost)
        }
        return image
    }

    private static func key(_ url: URL, _ fit: CGSize) -> NSURL {
        var c = URLComponents(url: url, resolvingAgainstBaseURL: false)!
        c.fragment = "\(Int(fit.width))x\(Int(fit.height))"
        return c.url! as NSURL
    }

    /// Decode `data` just big enough to cover (`fill`) or fit in `fit`.
    nonisolated private static func downsample(_ data: Data, fit: CGSize, fill: Bool) -> UIImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil),
              let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = props[kCGImagePropertyPixelWidth] as? CGFloat,
              let height = props[kCGImagePropertyPixelHeight] as? CGFloat,
              width > 0, height > 0 else { return nil }
        let ratios = (fit.width / width, fit.height / height)
        let scale = min(1, fill ? max(ratios.0, ratios.1) : min(ratios.0, ratios.1))
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: max(width, height) * scale,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
        return UIImage(cgImage: image)
    }
}

struct RemoteImage<Placeholder: View>: View {
    let url: URL?
    var contentMode: ContentMode = .fill
    @ViewBuilder var placeholder: () -> Placeholder

    @Environment(\.displayScale) private var displayScale
    @State private var image: UIImage?
    @State private var size: CGSize = .zero

    var body: some View {
        ZStack {
            if let image {
                Image(uiImage: image)
                    .resizable()
                    .aspectRatio(contentMode: contentMode)
            } else {
                placeholder()
            }
        }
        .onGeometryChange(for: CGSize.self) { $0.size } action: { size = $0 }
        .task(id: Load(url: url, size: size)) {
            guard let url, size.width > 0, size.height > 0 else { return }
            let fit = CGSize(width: size.width * displayScale, height: size.height * displayScale)
            if let hit = ImageLoader.shared.cached(url, fit: fit) { image = hit; return }
            image = await ImageLoader.shared.load(url, fit: fit, fill: contentMode == .fill)
        }
    }

    private struct Load: Equatable {
        let url: URL?
        let size: CGSize
    }
}

extension RemoteImage where Placeholder == ArtworkPlaceholder {
    init(url: URL?, title: String = "", contentMode: ContentMode = .fill) {
        self.init(url: url, contentMode: contentMode) { ArtworkPlaceholder(title: title) }
    }
}

struct ArtworkPlaceholder: View {
    var title: String

    var body: some View {
        ZStack {
            LinearGradient(
                colors: [Color(white: 0.22), Color(white: 0.12)],
                startPoint: .topLeading, endPoint: .bottomTrailing
            )
            Text(title)
                .font(.headline)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .padding()
        }
    }
}
