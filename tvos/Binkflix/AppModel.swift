import Foundation
import Observation
import Security

/// Which server we talk to and as whom. The server URL lives in defaults; the
/// device token in the keychain.
@Observable
final class AppModel {
    enum Phase {
        /// No server picked yet.
        case setup
        /// Server needs auth and we hold no token.
        case pairing(APIClient)
        case ready(APIClient)
    }

    private(set) var phase: Phase = .setup
    private(set) var login: String?
    /// Bumped whenever watch state may have changed (playback ended, marked
    /// watched), so screens showing progress reload.
    private(set) var watchGeneration = 0

    private static let serverKey = "server"

    init() {
        if let s = UserDefaults.standard.string(forKey: Self.serverKey), let url = URL(string: s) {
            let token = Keychain.token
            phase = .ready(APIClient(base: url, token: token))
            Task { await refresh() }
        }
    }

    var client: APIClient? {
        switch phase {
        case .setup: nil
        case let .pairing(c), let .ready(c): c
        }
    }

    /// Point the app at `url`. Asks the server whether it needs pairing.
    func connect(to url: URL) async throws {
        let anon = APIClient(base: url, token: nil)
        let info = try await anon.info()
        UserDefaults.standard.set(url.absoluteString, forKey: Self.serverKey)
        Keychain.token = nil
        if info.auth {
            phase = .pairing(anon)
        } else {
            phase = .ready(anon)
            await refresh()
        }
    }

    func paired(token: String, login: String) {
        guard let base = client?.base else { return }
        Keychain.token = token
        self.login = login
        phase = .ready(APIClient(base: base, token: token))
    }

    /// Re-check the saved token; drop to pairing if the server revoked it.
    func refresh() async {
        guard case let .ready(c) = phase else { return }
        do {
            login = try await c.me().login
        } catch APIError.unauthorized {
            unlink()
        } catch {
            // Offline or server down: keep the token, screens show the error.
        }
    }

    func unlink() {
        guard let base = client?.base else { return }
        Keychain.token = nil
        login = nil
        phase = .pairing(APIClient(base: base, token: nil))
    }

    func forgetServer() {
        UserDefaults.standard.removeObject(forKey: Self.serverKey)
        Keychain.token = nil
        login = nil
        phase = .setup
    }

    func watchStateChanged() {
        watchGeneration += 1
    }
}

enum Keychain {
    private static let account = "device-token"

    static var token: String? {
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
