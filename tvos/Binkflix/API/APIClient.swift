import Foundation

enum APIError: LocalizedError {
    case unauthorized
    case status(Int, String?)

    var errorDescription: String? {
        switch self {
        case .unauthorized: "This Apple TV is no longer linked."
        case let .status(code, body): "Server returned \(code)\(body.map { ": \($0)" } ?? "")"
        }
    }
}

/// Talks to one binkflix server, as one paired device (or anonymously when
/// the server runs without auth).
struct APIClient {
    let base: URL
    let token: String?

    static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    /// Headers every request needs, including the ones AVPlayer and the image
    /// loader make on their own.
    var authHeaders: [String: String] {
        token.map { ["Authorization": "Bearer \($0)"] } ?? [:]
    }

    func url(_ path: String, query: [String: String?] = [:]) -> URL {
        var c = URLComponents(url: base.appending(path: path), resolvingAgainstBaseURL: false)!
        let items = query.compactMap { k, v in v.map { URLQueryItem(name: k, value: $0) } }
        if !items.isEmpty { c.queryItems = items.sorted { $0.name < $1.name } }
        return c.url!
    }

    func request(_ url: URL, method: String = "GET", json: (any Encodable)? = nil) throws -> URLRequest {
        var req = URLRequest(url: url)
        req.httpMethod = method
        for (k, v) in authHeaders { req.setValue(v, forHTTPHeaderField: k) }
        if let json {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try JSONEncoder().encode(json)
        }
        return req
    }

    @discardableResult
    func send(_ req: URLRequest) async throws -> Data {
        let (data, resp) = try await URLSession.shared.data(for: req)
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        if code == 401 { throw APIError.unauthorized }
        guard (200..<300).contains(code) else {
            throw APIError.status(code, String(data: data, encoding: .utf8))
        }
        return data
    }

    func get<T: Decodable>(_ path: String, query: [String: String?] = [:]) async throws -> T {
        let data = try await send(request(url(path, query: query)))
        return try Self.decoder.decode(T.self, from: data)
    }

    func post(_ path: String, json: (any Encodable)? = nil) async throws {
        try await send(request(url(path), method: "POST", json: json))
    }

    func delete(_ path: String) async throws {
        try await send(request(url(path), method: "DELETE"))
    }
}

// MARK: - Endpoints

extension APIClient {
    func info() async throws -> ServerInfo { try await get("/api/info") }
    func me() async throws -> Me { try await get("/api/me") }
    func library() async throws -> Library { try await get("/api/library") }
    func continueWatching() async throws -> [ContinueItem] { try await get("/api/continue-watching") }
    func media(_ id: String) async throws -> Media { try await get("/api/media/\(id)") }
    func show(_ id: String) async throws -> ShowDetail { try await get("/api/shows/\(id)") }
    func tech(_ id: String) async throws -> MediaTechInfo { try await get("/api/media/\(id)/tech") }
    func markers(_ id: String) async throws -> MarkersResponse { try await get("/api/media/\(id)/markers.json") }
    func progress(_ id: String) async throws -> WatchProgress? { try await get("/api/media/\(id)/progress") }

    func search(_ q: String) async throws -> SearchResponse {
        try await get("/api/search", query: ["q": q, "limit": "40"])
    }

    func reportProgress(_ id: String, position: Double, duration: Double) async throws {
        struct Body: Encodable { let position_secs: Double; let duration_secs: Double }
        try await post("/api/media/\(id)/progress", json: Body(position_secs: position, duration_secs: duration))
    }

    func setWatched(_ id: String, _ watched: Bool) async throws {
        let path = "/api/media/\(id)/watched"
        if watched { try await post(path) } else { try await delete(path) }
    }

    func dismissContinueWatching(_ id: String) async throws {
        try await post("/api/continue-watching/dismiss/\(id)")
    }
}

// MARK: - Images

extension APIClient {
    func mediaImage(_ id: String) -> URL { url("/api/media/\(id)/image") }
    func mediaFanart(_ id: String) -> URL { url("/api/media/\(id)/fanart") }
    func showPoster(_ id: String) -> URL { url("/api/shows/\(id)/poster") }
    func showFanart(_ id: String) -> URL { url("/api/shows/\(id)/fanart") }
    func showClearlogo(_ id: String) -> URL { url("/api/shows/\(id)/clearlogo") }
}
