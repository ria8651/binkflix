import SwiftUI

struct HomeView: View {
    @Environment(AppModel.self) private var app
    @Environment(PlayerPresenter.self) private var player
    @State private var state: LoadState<(Library, [ContinueItem])> = .loading
    @State private var reload = 0

    var body: some View {
        Loadable(state: state, retry: { reload += 1 }) { library, continuing in
            ScrollView {
                VStack(alignment: .leading, spacing: 50) {
                    if !continuing.isEmpty {
                        Shelf(title: "Continue Watching") {
                            ForEach(continuing) { ContinueCard(item: $0) }
                        }
                    }
                    if !library.recentlyAdded.isEmpty {
                        Shelf(title: "Recently Added") {
                            ForEach(library.recentlyAdded) { RecentCard(item: $0) }
                        }
                    }
                    if !library.movies.isEmpty {
                        Shelf(title: "Movies") {
                            ForEach(library.movies) { MovieCard(movie: $0) }
                        }
                    }
                    if !library.shows.isEmpty {
                        Shelf(title: "Shows") {
                            ForEach(library.shows) { ShowCard(show: $0) }
                        }
                    }
                }
            }
        }
        .task(id: [app.watchGeneration, reload]) {
            guard let client = app.client else { return }
            let result = await Result {
                async let library = client.library()
                async let continuing = client.continueWatching()
                return try await (library, continuing)
            }
            state.apply(result)
        }
    }
}

struct ContinueCard: View {
    let item: ContinueItem
    @Environment(AppModel.self) private var app
    @Environment(PlayerPresenter.self) private var player

    var body: some View {
        Button {
            player.play(item.mediaId, app: app)
        } label: {
            CardLabel(
                url: item.kind == "episode" ? app.client?.mediaImage(item.mediaId) : app.client?.mediaFanart(item.mediaId),
                size: CardSize.landscape,
                title: item.showTitle ?? item.title,
                subtitle: subtitle,
                progress: item.durationSecs > 0 ? item.positionSecs / item.durationSecs : nil
            )
        }
        .buttonStyle(.borderless)
        .contextMenu {
            Button("Mark as Watched", systemImage: "checkmark") {
                Task {
                    try? await app.client?.setWatched(item.mediaId, true)
                    app.watchStateChanged()
                }
            }
            Button("Remove from Continue Watching", systemImage: "xmark") {
                Task {
                    try? await app.client?.dismissContinueWatching(item.mediaId)
                    app.watchStateChanged()
                }
            }
        }
    }

    private var subtitle: String {
        let left = max(item.durationSecs - item.positionSecs, 0)
        let remaining = "\(Int((left / 60).rounded(.up))) min left"
        if let tag = episodeTag(season: item.seasonNumber, episode: item.episodeNumber) {
            return "\(tag) · \(item.title) · \(remaining)"
        }
        return remaining
    }
}

struct RecentCard: View {
    let item: RecentItem
    @Environment(AppModel.self) private var app

    var body: some View {
        NavigationLink(value: item.showId.map(Destination.show) ?? .movie(item.mediaId)) {
            CardLabel(
                url: item.kind == "episode" ? app.client?.mediaImage(item.mediaId) : app.client?.mediaFanart(item.mediaId),
                size: CardSize.landscape,
                title: item.showTitle ?? item.title,
                subtitle: episodeTag(season: item.seasonNumber, episode: item.episodeNumber)
                    .map { "\($0) · \(item.title)" } ?? item.year.map(String.init)
            )
        }
        .buttonStyle(.borderless)
    }
}

private let gridColumns = [GridItem(.adaptive(minimum: CardSize.poster.width), spacing: 48, alignment: .top)]

struct MoviesView: View {
    @Environment(AppModel.self) private var app
    @State private var state: LoadState<[MovieSummary]> = .loading
    @State private var reload = 0

    var body: some View {
        Loadable(state: state, retry: { reload += 1 }) { movies in
            ScrollView {
                LazyVGrid(columns: gridColumns, spacing: 60) {
                    ForEach(movies) { MovieCard(movie: $0) }
                }
            }
        }
        .task(id: reload) {
            guard let client = app.client else { return }
            state.apply(await Result { try await client.library().movies })
        }
    }
}

struct ShowsView: View {
    @Environment(AppModel.self) private var app
    @State private var state: LoadState<[ShowSummary]> = .loading
    @State private var reload = 0

    var body: some View {
        Loadable(state: state, retry: { reload += 1 }) { shows in
            ScrollView {
                LazyVGrid(columns: gridColumns, spacing: 60) {
                    ForEach(shows) { ShowCard(show: $0) }
                }
            }
        }
        .task(id: reload) {
            guard let client = app.client else { return }
            state.apply(await Result { try await client.library().shows })
        }
    }
}

struct SearchView: View {
    @Environment(AppModel.self) private var app
    @State private var query = ""
    @State private var results: SearchResponse?

    var body: some View {
        ScrollView {
            if let results {
                VStack(alignment: .leading, spacing: 50) {
                    if !results.shows.isEmpty {
                        Shelf(title: "Shows") {
                            ForEach(results.shows) { ShowCard(show: $0) }
                        }
                    }
                    if !results.movies.isEmpty {
                        Shelf(title: "Movies") {
                            ForEach(results.movies) { MovieCard(movie: $0) }
                        }
                    }
                    if results.shows.isEmpty && results.movies.isEmpty {
                        Text("Nothing matches “\(query)”")
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity)
                    }
                }
            }
        }
        .searchable(text: $query)
        .task(id: query) {
            let q = query.trimmingCharacters(in: .whitespaces)
            guard !q.isEmpty else { results = nil; return }
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled, let client = app.client else { return }
            if let r = try? await client.search(q) { results = r }
        }
    }
}

struct SettingsView: View {
    @Environment(AppModel.self) private var app

    var body: some View {
        Form {
            Section("Server") {
                LabeledContent("Address", value: app.client?.base.absoluteString ?? "")
                if let login = app.login {
                    LabeledContent("Signed in as", value: login)
                }
            }
            Section {
                if app.client?.session != nil {
                    Button("Unlink This Apple TV", role: .destructive) { app.unlink() }
                }
                Button("Change Server") { app.forgetServer() }
            }
        }
        // Focused rows grow past the column; don't clip them.
        .scrollClipDisabled()
        .frame(maxWidth: 1200)
    }
}
