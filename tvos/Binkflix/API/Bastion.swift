import Foundation

/// Where the server says to pair (`/api/info`'s `bastion`).
struct BastionInfo: Codable {
    let origin: URL
    let service: String
}

struct DeviceCode: Decodable {
    let deviceCode: String
    let userCode: String
    let verificationUri: String
    let verificationUriComplete: String
    let expiresIn: Int
    let interval: Int
}

struct BastionTokens: Decodable {
    let accessToken: String
    let refreshToken: String?
}

/// An OAuth error body (`{"error": "authorization_pending", …}`).
struct OAuthError: Error, Decodable, LocalizedError {
    let error: String
    let errorDescription: String?
}

/// Signs this Apple TV in to one binkflix server through bastion's device
/// sign-in (RFC 8628). Pairing yields a refresh token; signing in swaps it
/// for a JWT and trades that at the server's `/auth/bastion` for the same
/// session cookie a browser gets. URLSession, the image loader and AVPlayer
/// all send the cookie from shared storage on their own.
final class BastionSession {
    let info: BastionInfo
    let server: URL
    private var refreshToken: String?
    private var signingIn: Task<Void, Error>?

    init(info: BastionInfo, server: URL, refreshToken: String?) {
        self.info = info
        self.server = server
        self.refreshToken = refreshToken
    }

    var isPaired: Bool { refreshToken != nil }

    /// A fresh session cookie. Concurrent callers (every screen hitting a 401
    /// after a server restart) share one sign-in. Throws
    /// `APIError.unauthorized` once bastion no longer honours the device.
    func signIn() async throws {
        if let signingIn { return try await signingIn.value }
        let task = Task { try await self.exchange(self.refresh()) }
        signingIn = task
        defer { signingIn = nil }
        try await task.value
    }

    /// Finish pairing with the tokens the device code yielded.
    func paired(_ tokens: BastionTokens) async throws {
        refreshToken = tokens.refreshToken
        Keychain.refreshToken = tokens.refreshToken
        try await exchange(tokens.accessToken)
    }

    func startPairing(name: String) async throws -> DeviceCode {
        try await post("/api/device/code", ["client_id": info.service, "name": name])
    }

    /// Tokens once approved. Throws `OAuthError` (`authorization_pending`,
    /// `slow_down`, `expired_token`, `access_denied`, …) until then.
    func poll(_ code: DeviceCode) async throws -> BastionTokens {
        try await post("/api/device/token", [
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "client_id": info.service,
            "device_code": code.deviceCode,
        ])
    }

    private func refresh() async throws -> String {
        guard let refreshToken else { throw APIError.unauthorized }
        do {
            let tokens: BastionTokens = try await post("/api/device/token", [
                "grant_type": "refresh_token",
                "client_id": info.service,
                "refresh_token": refreshToken,
            ])
            return tokens.accessToken
        } catch let e as OAuthError where e.error == "invalid_grant" || e.error == "invalid_client" {
            throw APIError.unauthorized
        }
    }

    /// `/auth/bastion` sets the cookie and redirects to the web app's home
    /// page, which we don't need.
    private func exchange(_ jwt: String) async throws {
        var c = URLComponents(url: server.appending(path: "/auth/bastion"), resolvingAgainstBaseURL: false)!
        c.queryItems = [URLQueryItem(name: "bastion_token", value: jwt)]
        let (data, resp) = try await Self.noRedirects.data(from: c.url!)
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        if code == 401 { throw APIError.unauthorized }
        guard (200..<400).contains(code) else {
            throw APIError.status(code, String(data: data, encoding: .utf8))
        }
    }

    private func post<T: Decodable>(_ path: String, _ form: [String: String]) async throws -> T {
        var req = URLRequest(url: info.origin.appending(path: path))
        req.httpMethod = "POST"
        req.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
        let unreserved = CharacterSet(charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")
        let body = form.map { k, v in
            "\(k)=\(v.addingPercentEncoding(withAllowedCharacters: unreserved) ?? "")"
        }
        req.httpBody = Data(body.joined(separator: "&").utf8)
        let (data, resp) = try await URLSession.shared.data(for: req)
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        guard (200..<300).contains(code) else {
            if let e = try? APIClient.decoder.decode(OAuthError.self, from: data) { throw e }
            throw APIError.status(code, String(data: data, encoding: .utf8))
        }
        return try APIClient.decoder.decode(T.self, from: data)
    }

    private static let noRedirects = URLSession(configuration: .default, delegate: NoRedirects(), delegateQueue: nil)

    private nonisolated final class NoRedirects: NSObject, URLSessionTaskDelegate, Sendable {
        func urlSession(
            _ session: URLSession, task: URLSessionTask,
            willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
            completionHandler: @escaping @Sendable (URLRequest?) -> Void
        ) { completionHandler(nil) }
    }
}
