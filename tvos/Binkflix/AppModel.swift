import Foundation
import Observation
import Security

/// Which server we talk to and as whom. The server URL and where to pair
/// live in defaults; bastion's refresh token in the keychain.
@Observable
final class AppModel {
    enum Phase {
        /// No server picked yet.
        case setup
        /// Server needs auth and this Apple TV isn't paired.
        case pairing(BastionSession)
        case ready(APIClient)
    }

    private(set) var phase: Phase = .setup
    private(set) var login: String?
    /// Bumped whenever watch state may have changed (playback ended, marked
    /// watched), so screens showing progress reload.
    private(set) var watchGeneration = 0

    private static let serverKey = "server"
    private static let bastionKey = "bastion"

    init() {
        guard let s = UserDefaults.standard.string(forKey: Self.serverKey), let url = URL(string: s) else { return }
        guard let bastion = Self.savedBastion else {
            phase = .ready(APIClient(base: url, session: nil))
            Task { await refresh() }
            return
        }
        let session = BastionSession(info: bastion, server: url, refreshToken: Keychain.refreshToken)
        if session.isPaired {
            phase = .ready(APIClient(base: url, session: session))
            Task { await refresh() }
        } else {
            phase = .pairing(session)
        }
    }

    var client: APIClient? {
        if case let .ready(c) = phase { c } else { nil }
    }

    var server: URL? {
        switch phase {
        case .setup: nil
        case let .pairing(s): s.server
        case let .ready(c): c.base
        }
    }

    /// Point the app at `url`. Asks the server whether it needs pairing.
    func connect(to url: URL) async throws {
        let info = try await APIClient(base: url, session: nil).info()
        signOut()
        UserDefaults.standard.set(url.absoluteString, forKey: Self.serverKey)
        Self.savedBastion = info.bastion
        if let bastion = info.bastion {
            phase = .pairing(BastionSession(info: bastion, server: url, refreshToken: nil))
        } else {
            phase = .ready(APIClient(base: url, session: nil))
            await refresh()
        }
    }

    /// Bastion approved the code `session` was pairing with.
    func paired(_ session: BastionSession, _ tokens: BastionTokens) async throws {
        try await session.paired(tokens)
        phase = .ready(APIClient(base: session.server, session: session))
        await refresh()
    }

    /// Re-check the session; drop to pairing if bastion signed this device out.
    func refresh() async {
        guard case let .ready(c) = phase else { return }
        do {
            login = try await c.me().login
        } catch APIError.unauthorized {
            unlink()
        } catch {
            // Offline or server down: stay signed in, screens show the error.
        }
    }

    /// Forget this device's credentials. Bastion keeps listing it on the
    /// account page, where it can be revoked.
    func unlink() {
        guard let server else { return }
        guard let bastion = Self.savedBastion else {
            // Saved before the server turned auth on: ask it again.
            Task { try? await connect(to: server) }
            return
        }
        signOut()
        phase = .pairing(BastionSession(info: bastion, server: server, refreshToken: nil))
    }

    func forgetServer() {
        signOut()
        UserDefaults.standard.removeObject(forKey: Self.serverKey)
        Self.savedBastion = nil
        phase = .setup
    }

    private func signOut() {
        login = nil
        Keychain.refreshToken = nil
        guard let server else { return }
        Task { _ = try? await URLSession.shared.data(from: server.appending(path: "/auth/logout")) }
        for cookie in HTTPCookieStorage.shared.cookies(for: server) ?? [] {
            HTTPCookieStorage.shared.deleteCookie(cookie)
        }
    }

    private static var savedBastion: BastionInfo? {
        get {
            UserDefaults.standard.data(forKey: bastionKey).flatMap { try? JSONDecoder().decode(BastionInfo.self, from: $0) }
        }
        set {
            UserDefaults.standard.set(newValue.flatMap { try? JSONEncoder().encode($0) }, forKey: bastionKey)
        }
    }

    func watchStateChanged() {
        watchGeneration += 1
    }
}

enum Keychain {
    private static let account = "bastion-refresh-token"

    static var refreshToken: String? {
        get {
            var out: CFTypeRef?
            let query: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrAccount as String: account,
                kSecReturnData as String: true,
            ]
            guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess,
                  let data = out as? Data else { return nil }
            return String(data: data, encoding: .utf8)
        }
        set {
            let query: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrAccount as String: account,
            ]
            SecItemDelete(query as CFDictionary)
            guard let newValue else { return }
            var add = query
            add[kSecValueData as String] = Data(newValue.utf8)
            add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
            SecItemAdd(add as CFDictionary, nil)
        }
    }
}
