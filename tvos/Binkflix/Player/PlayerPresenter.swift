import AVKit
import Observation
import UIKit

/// Starts playback. Presents AVPlayerViewController modally over whatever is
/// on screen, so the remote's Menu button dismisses it the system way.
@Observable
final class PlayerPresenter {
    private var session: PlaybackSession?
    /// What was on screen when the player last closed, so the page behind it
    /// can put focus back there (it may be a later episode than the one
    /// picked, after Next Episode).
    private(set) var lastPlayed: String?

    /// Play `id`, from `from` seconds, or from the server's resume point.
    func play(_ id: String, app: AppModel, from: Double? = nil) {
        guard session == nil, let client = app.client, let host = Self.topViewController() else { return }
        let session = PlaybackSession(client: client, mediaId: id, startAt: from)
        session.onFinish = { [weak self, weak app] lastId in
            self?.lastPlayed = lastId
            self?.session = nil
            app?.watchStateChanged()
        }
        self.session = session
        session.present(from: host)
    }

    private static func topViewController() -> UIViewController? {
        let scene = UIApplication.shared.connectedScenes.first { $0.activationState == .foregroundActive } as? UIWindowScene
        var top = scene?.keyWindow?.rootViewController
        while let next = top?.presentedViewController { top = next }
        return top
    }
}
