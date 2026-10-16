import Foundation

// Mirrors of the server's DTOs (`src/types.rs`). Decoded with
// `.convertFromSnakeCase`, so `show_id` arrives as `showId`.

struct ServerInfo: Decodable {
    let name: String
    let apiVersion: Int
    let auth: Bool
    /// Where to pair when `auth` is on.
    let bastion: BastionInfo?
}

struct Me: Decodable {
    let login: String
}

struct MovieSummary: Decodable, Hashable, Identifiable {
    let id: String
    let title: String
    let year: Int?
}

struct ShowSummary: Decodable, Hashable, Identifiable {
    let id: String
    let title: String
    let year: Int?
    let episodeCount: Int
}

struct RecentItem: Decodable, Hashable, Identifiable {
    let mediaId: String
    let kind: String
    let title: String
    let showId: String?
    let showTitle: String?
    let seasonNumber: Int?
    let episodeNumber: Int?
    let year: Int?

    var id: String { mediaId }
}

struct Library: Decodable {
    let movies: [MovieSummary]
    let shows: [ShowSummary]
    let recentlyAdded: [RecentItem]
}

struct SearchResponse: Decodable {
    let movies: [MovieSummary]
    let shows: [ShowSummary]
}

struct Media: Decodable, Identifiable {
    let id: String
    let kind: String
    let title: String
    let year: Int?
    let plot: String?
    let runtimeMinutes: Int?
    let showId: String?
    let seasonNumber: Int?
    let episodeNumber: Int?
    let hasFanart: Bool
    let rating: Double?
    let mpaa: String?
    let tagline: String?
    let director: String?
    let genres: [String]
}

struct Show: Decodable {
    let id: String
    let title: String
    let year: Int?
    let plot: String?
    let hasClearlogo: Bool
    let hasFanart: Bool
    let rating: Double?
    let mpaa: String?
    let status: String?
    let genres: [String]
}

struct EpisodeSummary: Decodable, Hashable, Identifiable {
    let id: String
    let seasonNumber: Int
    let episodeNumber: Int
    let title: String
    let plot: String?
    let runtimeMinutes: Int?
    let positionSecs: Double
    let durationSecs: Double
    let completed: Int

    var isCompleted: Bool { completed != 0 }
    var progress: Double? {
        guard !isCompleted, positionSecs > 0, durationSecs > 0 else { return nil }
        return positionSecs / durationSecs
    }
}

struct Season: Decodable, Hashable, Identifiable {
    let number: Int
    let episodes: [EpisodeSummary]

    var id: Int { number }
    var name: String { number == 0 ? "Specials" : "Season \(number)" }
}

struct ShowDetail: Decodable {
    let show: Show
    let seasons: [Season]
}

struct ContinueItem: Decodable, Hashable, Identifiable {
    let mediaId: String
    let kind: String
    let title: String
    let showId: String?
    let showTitle: String?
    let seasonNumber: Int?
    let episodeNumber: Int?
    let year: Int?
    let positionSecs: Double
    let durationSecs: Double

    var id: String { mediaId }
}

struct WatchProgress: Decodable {
    let positionSecs: Double
    let durationSecs: Double
    let completed: Bool
    let resumeSecs: Double
}

struct MediaTechInfo: Decodable {
    struct Video: Decodable {
        let codec: String
        let pixFmt: String?
        let height: Int?
    }
    let durationSeconds: Double?
    let video: Video?
}

enum MarkerKind: String, Decodable {
    case intro, recap, outro, credits, chapter
}

struct Marker: Decodable {
    let kind: MarkerKind
    let startSecs: Double
    let endSecs: Double
}

struct MarkersResponse: Decodable {
    let markers: [Marker]
}

/// "S1E2" for an episode, `nil` otherwise.
func episodeTag(season: Int?, episode: Int?) -> String? {
    guard let season, let episode else { return nil }
    return "S\(season)E\(episode)"
}
