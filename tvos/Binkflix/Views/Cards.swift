import SwiftUI

enum CardSize {
    static let poster = CGSize(width: 250, height: 375)
    static let landscape = CGSize(width: 460, height: 259)
}

/// A titled horizontal row of cards.
struct Shelf<Content: View>: View {
    let title: String
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 24) {
            Text(title)
                .font(.title3.weight(.semibold))
                .foregroundStyle(.secondary)
            ScrollView(.horizontal) {
                LazyHStack(alignment: .top, spacing: 48) {
                    content()
                }
                .padding(.vertical, 20)
            }
            .scrollClipDisabled()
        }
        .focusSection()
    }
}

/// Artwork with a caption under it, as a button label with
/// `.buttonStyle(.borderless)`. The body is two sibling views (no stack) so
/// the style sees artwork + title and does the TV-app focus look itself:
/// the artwork lifts and the caption slides down out of its way.
struct CardLabel: View {
    let url: URL?
    let size: CGSize
    let title: String
    var subtitle: String?
    /// Longer text under the subtitle, e.g. an episode's plot.
    var detail: String?
    /// 0…1 watch progress drawn across the bottom of the artwork.
    var progress: Double?
    var watched = false

    var body: some View {
        RemoteImage(url: url, title: title)
            .frame(width: size.width, height: size.height)
            .overlay(alignment: .bottom) {
                if let progress {
                    ProgressBar(value: progress).padding(14)
                }
            }
            .overlay(alignment: .topTrailing) {
                if watched {
                    Image(systemName: "checkmark.circle.fill")
                        .font(.title3)
                        .symbolRenderingMode(.palette)
                        .foregroundStyle(.black, .white)
                        .padding(12)
                }
            }
            .clipShape(.rect(cornerRadius: 12))
            .hoverEffect(.highlight)
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
                .font(.caption)
                .lineLimit(1)
            if let subtitle {
                Text(subtitle)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            if let detail {
                Text(detail)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(3, reservesSpace: true)
                    .padding(.top, 6)
            }
        }
        .frame(width: size.width, alignment: .leading)
    }
}

struct ProgressBar: View {
    let value: Double

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(.black.opacity(0.5))
                Capsule().fill(.white)
                    .frame(width: geo.size.width * min(max(value, 0), 1))
            }
        }
        .frame(height: 6)
    }
}

struct MovieCard: View {
    let movie: MovieSummary
    @Environment(AppModel.self) private var app

    var body: some View {
        NavigationLink(value: Destination.movie(movie.id)) {
            CardLabel(
                url: app.client?.mediaImage(movie.id),
                size: CardSize.poster,
                title: movie.title,
                subtitle: movie.year.map(String.init)
            )
        }
        .buttonStyle(.borderless)
    }
}

struct ShowCard: View {
    let show: ShowSummary
    @Environment(AppModel.self) private var app

    var body: some View {
        NavigationLink(value: Destination.show(show.id)) {
            CardLabel(
                url: app.client?.showPoster(show.id),
                size: CardSize.poster,
                title: show.title,
                subtitle: show.year.map(String.init)
            )
        }
        .buttonStyle(.borderless)
    }
}

/// Loading / error / content switch for a screen fed by one request.
struct Loadable<Value, Content: View>: View {
    let state: LoadState<Value>
    var retry: () -> Void
    @ViewBuilder var content: (Value) -> Content

    var body: some View {
        switch state {
        case .loading:
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
        case let .failed(message):
            VStack(spacing: 30) {
                Text(message).foregroundStyle(.secondary).multilineTextAlignment(.center)
                Button("Try Again", action: retry)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case let .loaded(value):
            content(value)
        }
    }
}

enum LoadState<Value> {
    case loading
    case failed(String)
    case loaded(Value)

    var value: Value? {
        if case let .loaded(v) = self { v } else { nil }
    }

    /// Take a fetch's result. A failed reload keeps what's already on screen.
    mutating func apply(_ result: Result<Value, any Error>) {
        switch result {
        case let .success(v): self = .loaded(v)
        case let .failure(e) where value == nil: self = .failed(e.localizedDescription)
        case .failure: break
        }
    }
}

extension Result where Failure == any Error {
    init(catching body: () async throws -> Success) async {
        do { self = .success(try await body()) } catch { self = .failure(error) }
    }
}
