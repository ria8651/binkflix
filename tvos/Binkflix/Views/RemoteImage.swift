import SwiftUI
import UIKit

/// Loads artwork with the device's bearer token (AsyncImage can't send
/// headers). Decoded images are kept in memory; the bytes in URLCache.
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

    func cached(_ url: URL) -> UIImage? {
        cache.object(forKey: url as NSURL)
    }

    func load(_ url: URL, headers: [String: String]) async -> UIImage? {
        if let hit = cached(url) { return hit }
        if let task = inflight[url] { return await task.value }
        let session = session
        let task = Task<UIImage?, Never> {
            var req = URLRequest(url: url)
            for (k, v) in headers { req.setValue(v, forHTTPHeaderField: k) }
            guard let (data, resp) = try? await session.data(for: req),
                  (resp as? HTTPURLResponse)?.statusCode == 200,
                  let image = UIImage(data: data) else { return nil }
            return await image.byPreparingForDisplay() ?? image
        }
        inflight[url] = task
        let image = await task.value
        inflight[url] = nil
        if let image {
            let cost = Int(image.size.width * image.size.height * image.scale * image.scale * 4)
            cache.setObject(image, forKey: url as NSURL, cost: cost)
        }
        return image
    }
}

struct RemoteImage<Placeholder: View>: View {
    let url: URL?
    var contentMode: ContentMode = .fill
    @ViewBuilder var placeholder: () -> Placeholder

    @Environment(AppModel.self) private var app
    @State private var image: UIImage?

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
        .task(id: url) {
            guard let url else { image = nil; return }
            if let hit = ImageLoader.shared.cached(url) { image = hit; return }
            image = nil
            image = await ImageLoader.shared.load(url, headers: app.client?.authHeaders ?? [:])
        }
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
