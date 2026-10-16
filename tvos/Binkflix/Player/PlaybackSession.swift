import AVKit
import UIKit
import VideoToolbox

/// One stint in the player: loads an item (and the episodes after it),
/// reports progress, offers skip actions from markers, and tears down when
/// the user leaves.
final class PlaybackSession: NSObject, AVPlayerViewControllerDelegate {
    private let client: APIClient
    private let controller = AVPlayerViewController()
    private let player = AVPlayer()
    private var current: Item?
    private var timeObserver: Any?
    private var statusObservation: NSKeyValueObservation?
    private var endObserver: NSObjectProtocol?
    private var lastReport = Date.distantPast
    private var finished = false

    /// Called once the player is gone, with the media it was showing last.
    var onFinish: ((String?) -> Void)?

    /// How often progress is saved while playing.
    private static let reportInterval: TimeInterval = 10

    private struct Item {
        let media: Media
        let markers: [Marker]
        /// Next episode in the show, for "Next Episode" and autoplay.
        let next: EpisodeSummary?
        let duration: Double?
        let resume: Double
    }

    init(client: APIClient, mediaId: String, startAt: Double?) {
        self.client = client
        super.init()
        controller.player = player
        controller.delegate = self
        timeObserver = player.addPeriodicTimeObserver(
            forInterval: CMTime(seconds: 1, preferredTimescale: 600), queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
        Task { await load(mediaId, startAt: startAt) }
    }

    func present(from host: UIViewController) {
        host.present(controller, animated: true)
    }

    // MARK: Loading

    private func load(_ id: String, startAt: Double?) async {
        do {
            async let media = client.media(id)
            async let tech = client.tech(id)
            async let progress = client.progress(id)
            async let markers = try? client.markers(id).markers
            let (m, t, p, mk) = try await (media, tech, progress, markers)
            let next = await nextEpisode(after: m)
            let resume = startAt ?? p?.resumeSecs ?? 0
            let item = Item(media: m, markers: mk ?? [], next: next, duration: t.durationSeconds, resume: resume)
            start(item, mode: Self.mode(for: t))
        } catch {
            fail(error.localizedDescription)
        }
    }

    private func nextEpisode(after media: Media) async -> EpisodeSummary? {
        guard let showId = media.showId, let detail = try? await client.show(showId) else { return nil }
        let episodes = detail.seasons.filter { $0.number > 0 }.flatMap(\.episodes)
        guard let i = episodes.firstIndex(where: { $0.id == media.id }), i + 1 < episodes.count else { return nil }
        return episodes[i + 1]
    }

    /// Sources this box can decode are only repackaged; anything else is
    /// transcoded by the server (to H.264, at most 1080p). Hardware HEVC
    /// marks a 4K-capable box (A10X and later); the Apple TV HD has neither.
    private static func mode(for tech: MediaTechInfo) -> String {
        guard let video = tech.video else { return "remux" }
        let pix = video.pixFmt ?? "yuv420p"
        let hevc = VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC)
        let fits = hevc || (video.height ?? 0) <= 1080
        switch video.codec {
        case "h264": return fits && ["yuv420p", "yuvj420p"].contains(pix) ? "remux" : "transcode"
        case "hevc": return hevc && ["yuv420p", "yuvj420p", "yuv420p10le"].contains(pix) ? "remux" : "transcode"
        default: return "transcode"
        }
    }

    private func start(_ item: Item, mode: String) {
        // The user may have backed out while this was loading.
        guard !finished else { return }
        current = item
        var query: [String: String?] = ["mode": mode]
        if item.resume > 0 { query["t"] = String(format: "%.3f", item.resume) }
        let url = client.url("/api/media/\(item.media.id)/hls/master.m3u8", query: query)
        let asset = AVURLAsset(url: url, options: ["AVURLAssetHTTPHeaderFieldsKey": client.authHeaders])
        let playerItem = AVPlayerItem(asset: asset)
        playerItem.externalMetadata = metadata(for: item.media)

        statusObservation = playerItem.observe(\.status) { [weak self] observed, _ in
            Task { @MainActor in self?.statusChanged(observed) }
        }
        if let endObserver { NotificationCenter.default.removeObserver(endObserver) }
        endObserver = NotificationCenter.default.addObserver(
            forName: AVPlayerItem.didPlayToEndTimeNotification, object: playerItem, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.reachedEnd() }
        }

        controller.contextualActions = []
        lastReport = .now
        player.replaceCurrentItem(with: playerItem)
        player.play()
        loadArtwork(for: item.media, into: playerItem)
    }

    private func statusChanged(_ item: AVPlayerItem) {
        switch item.status {
        case .readyToPlay:
            // EXT-X-START should have put us there; seek if the player ignored it.
            if let resume = current?.resume, resume > 0, abs(player.currentTime().seconds - resume) > 10 {
                player.seek(to: CMTime(seconds: resume, preferredTimescale: 600))
            }
        case .failed:
            fail(item.error?.localizedDescription ?? "Playback failed.")
        default:
            break
        }
    }

    private func metadata(for media: Media) -> [AVMetadataItem] {
        func entry(_ id: AVMetadataIdentifier, _ value: String) -> AVMetadataItem {
            let item = AVMutableMetadataItem()
            item.identifier = id
            item.value = value as NSString
            item.extendedLanguageTag = "und"
            return item
        }
        var items: [AVMetadataItem] = []
        if let tag = episodeTag(season: media.seasonNumber, episode: media.episodeNumber) {
            items.append(entry(.commonIdentifierTitle, "\(tag) · \(media.title)"))
        } else {
            items.append(entry(.commonIdentifierTitle, media.title))
        }
        if let plot = media.plot { items.append(entry(.commonIdentifierDescription, plot)) }
        return items
    }

    private func loadArtwork(for media: Media, into item: AVPlayerItem) {
        let url = media.showId.map(client.showPoster) ?? client.mediaImage(media.id)
        Task {
            guard let image = await ImageLoader.shared.load(url, fit: CGSize(width: 600, height: 900), fill: false, headers: client.authHeaders),
                  let data = image.jpegData(compressionQuality: 0.8) else { return }
            let art = AVMutableMetadataItem()
            art.identifier = .commonIdentifierArtwork
            art.value = data as NSData
            art.dataType = kCMMetadataBaseDataType_JPEG as String
            art.extendedLanguageTag = "und"
            item.externalMetadata.append(art)
        }
    }

    // MARK: Playing

    private func tick() {
        guard let item = current else { return }
        let now = player.currentTime().seconds
        guard now.isFinite else { return }
        updateActions(item, at: now)
        if player.rate > 0, Date.now.timeIntervalSince(lastReport) >= Self.reportInterval {
            report(item)
        }
    }

    private var duration: Double? {
        let d = player.currentItem?.duration.seconds
        if let d, d.isFinite, d > 0 { return d }
        return current?.duration
    }

    private func report(_ item: Item, position: Double? = nil) {
        lastReport = .now
        guard let duration else { return }
        let position = position ?? player.currentTime().seconds
        guard position.isFinite, position >= 0 else { return }
        let client = client
        Task { try? await client.reportProgress(item.media.id, position: min(position, duration), duration: duration) }
    }

    /// Offer Skip Intro / Skip Recap while inside one, and Next Episode (or
    /// Skip Credits) over the outro.
    private func updateActions(_ item: Item, at now: Double) {
        var title: String?
        var handler: () -> Void = {}
        if let marker = item.markers.first(where: { $0.kind != .chapter && now >= $0.startSecs && now < $0.endSecs - 1 }) {
            switch marker.kind {
            case .intro, .recap:
                title = marker.kind == .intro ? "Skip Intro" : "Skip Recap"
                handler = { [weak self] in self?.player.seek(to: CMTime(seconds: marker.endSecs, preferredTimescale: 600)) }
            case .outro, .credits:
                if let next = item.next {
                    title = "Next Episode"
                    handler = { [weak self] in self?.playNext(next) }
                } else {
                    title = "Skip Credits"
                    handler = { [weak self] in self?.player.seek(to: CMTime(seconds: marker.endSecs, preferredTimescale: 600)) }
                }
            case .chapter:
                break
            }
        }
        if controller.contextualActions.first?.title != title {
            controller.contextualActions = title.map { [UIAction(title: $0) { _ in handler() }] } ?? []
        }
    }

    private func reachedEnd() {
        guard let item = current else { return }
        if let duration { report(item, position: duration) }
        if let next = item.next {
            playNext(next)
        } else {
            controller.dismiss(animated: true) { [weak self] in self?.finish() }
        }
    }

    private func playNext(_ next: EpisodeSummary) {
        if let item = current { report(item) }
        player.pause()
        Task { await load(next.id, startAt: 0) }
    }

    private func fail(_ message: String) {
        guard !finished else { return }
        let alert = UIAlertController(title: "Can't Play", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default) { [weak self] _ in
            self?.controller.dismiss(animated: true) { self?.finish() }
        })
        controller.present(alert, animated: true)
    }

    private func finish() {
        guard !finished else { return }
        finished = true
        if let item = current, player.currentTime().seconds > 5 { report(item) }
        player.pause()
        if let timeObserver { player.removeTimeObserver(timeObserver) }
        if let endObserver { NotificationCenter.default.removeObserver(endObserver) }
        statusObservation = nil
        player.replaceCurrentItem(with: nil)
        onFinish?(current?.media.id)
    }

    // MARK: AVPlayerViewControllerDelegate

    func playerViewControllerDidEndDismissalTransition(_ playerViewController: AVPlayerViewController) {
        finish()
    }
}
