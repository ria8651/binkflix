import SwiftUI

/// Full-bleed artwork behind a detail page, darkened towards the text.
struct Backdrop: View {
    let url: URL?

    var body: some View {
        RemoteImage(url: url) { Color.black }
            .overlay {
                LinearGradient(
                    stops: [
                        .init(color: .black.opacity(0.95), location: 0),
                        .init(color: .black.opacity(0.55), location: 0.55),
                        .init(color: .black.opacity(0.2), location: 1),
                    ],
                    startPoint: .bottomLeading, endPoint: .topTrailing
                )
            }
            .ignoresSafeArea()
    }
}

struct MetaLine: View {
    let parts: [String?]

    var body: some View {
        Text(parts.compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "))
            .font(.callout)
            .foregroundStyle(.secondary)
    }
}

private func runtime(_ minutes: Int?) -> String? {
    guard let minutes, minutes > 0 else { return nil }
    return minutes >= 60 ? "\(minutes / 60) hr \(minutes % 60) min" : "\(minutes) min"
}

private func rating(_ r: Double?) -> String? {
    r.map { String(format: "★ %.1f", $0) }
}

// MARK: - Movie

struct MovieDetailView: View {
    let id: String

    @Environment(AppModel.self) private var app
    @Environment(PlayerPresenter.self) private var player
    @State private var state: LoadState<(Media, WatchProgress?)> = .loading
    @State private var reload = 0

    var body: some View {
        Loadable(state: state, retry: { reload += 1 }) { media, progress in
            ZStack(alignment: .bottomLeading) {
                Backdrop(url: media.hasFanart ? app.client?.mediaFanart(id) : nil)
                HStack(alignment: .bottom, spacing: 60) {
                    RemoteImage(url: app.client?.mediaImage(id), title: media.title)
                        .frame(width: 340, height: 510)
                        .clipShape(.rect(cornerRadius: 16))
                    VStack(alignment: .leading, spacing: 24) {
                        Text(media.title)
                            .font(.largeTitle.bold())
                            .lineLimit(2)
                        MetaLine(parts: [
                            media.year.map(String.init), runtime(media.runtimeMinutes),
                            media.mpaa, rating(media.rating), media.genres.prefix(3).joined(separator: ", "),
                        ])
                        if let plot = media.plot {
                            Text(plot)
                                .font(.callout)
                                .lineLimit(5)
                                .frame(maxWidth: 1100, alignment: .leading)
                        }
                        PlayButtons(id: id, title: media.title, progress: progress)
                    }
                }
                .padding(.bottom, 60)
            }
        }
        .task(id: [app.watchGeneration, reload]) {
            guard let client = app.client else { return }
            let result = await Result {
                async let media = client.media(id)
                async let progress = client.progress(id)
                return try await (media, progress)
            }
            state.apply(result)
        }
    }
}

/// Play/Resume, Start Over, and the watched toggle for a single item.
struct PlayButtons: View {
    let id: String
    let title: String
    let progress: WatchProgress?

    @Environment(AppModel.self) private var app
    @Environment(PlayerPresenter.self) private var player

    var body: some View {
        let resume = progress?.resumeSecs ?? 0
        let watched = progress?.completed ?? false
        HStack(spacing: 30) {
            Button {
                player.play(id, app: app)
            } label: {
                Label(resume > 0 ? "Resume \(timestamp(resume))" : "Play", systemImage: "play.fill")
            }
            if resume > 0 {
                Button {
                    player.play(id, app: app, from: 0)
                } label: {
                    Label("Start Over", systemImage: "arrow.counterclockwise")
                }
            }
            Button {
                Task {
                    try? await app.client?.setWatched(id, !watched)
                    app.watchStateChanged()
                }
            } label: {
                Label(watched ? "Watched" : "Mark Watched", systemImage: watched ? "checkmark.circle.fill" : "checkmark.circle")
            }
        }
        .padding(.top, 20)
        .focusSection()
    }
}

func timestamp(_ secs: Double) -> String {
    let s = Int(secs)
    return s >= 3600
        ? String(format: "%d:%02d:%02d", s / 3600, s / 60 % 60, s % 60)
        : String(format: "%d:%02d", s / 60, s % 60)
}

// MARK: - Show

struct ShowDetailView: View {
    let id: String

    @Environment(AppModel.self) private var app
    @Environment(PlayerPresenter.self) private var player
    @State private var state: LoadState<ShowDetail> = .loading
    @State private var reload = 0
    @State private var seasonNumber: Int?

    var body: some View {
        Loadable(state: state, retry: { reload += 1 }) { detail in
            ZStack {
                Backdrop(url: detail.show.hasFanart ? app.client?.showFanart(id) : nil)
                ScrollView {
                    VStack(alignment: .leading, spacing: 40) {
                        header(detail)
                            .frame(minHeight: 560, alignment: .bottomLeading)
                        seasons(detail)
                    }
                }
            }
        }
        .task(id: [app.watchGeneration, reload]) {
            guard let client = app.client else { return }
            let result = await Result { try await client.show(id) }
            state.apply(result)
            if seasonNumber == nil, let detail = state.value {
                seasonNumber = upNext(detail)?.seasonNumber ?? detail.seasons.first?.number
            }
        }
    }

    @ViewBuilder
    private func header(_ detail: ShowDetail) -> some View {
        let show = detail.show
        VStack(alignment: .leading, spacing: 24) {
            if show.hasClearlogo {
                RemoteImage(url: app.client?.showClearlogo(id), contentMode: .fit) {
                    Text(show.title).font(.largeTitle.bold())
                }
                .frame(maxWidth: 700, maxHeight: 220, alignment: .bottomLeading)
            } else {
                Text(show.title).font(.largeTitle.bold())
            }
            MetaLine(parts: [
                show.year.map(String.init), show.mpaa, rating(show.rating), show.status,
                show.genres.prefix(3).joined(separator: ", "),
            ])
            if let plot = show.plot {
                Text(plot)
                    .font(.callout)
                    .lineLimit(4)
                    .frame(maxWidth: 1100, alignment: .leading)
            }
            if let next = upNext(detail) {
                Button {
                    player.play(next.id, app: app)
                } label: {
                    let tag = "S\(next.seasonNumber) E\(next.episodeNumber)"
                    Label(next.progress != nil ? "Resume \(tag)" : "Play \(tag)", systemImage: "play.fill")
                }
                .padding(.top, 20)
            }
        }
        .focusSection()
    }

    @ViewBuilder
    private func seasons(_ detail: ShowDetail) -> some View {
        if detail.seasons.count > 1 {
            ScrollView(.horizontal) {
                HStack(spacing: 20) {
                    ForEach(detail.seasons) { season in
                        Button(season.name) { seasonNumber = season.number }
                            .buttonStyle(.bordered)
                            .tint(season.number == seasonNumber ? .white : nil)
                    }
                }
                .padding(.vertical, 20)
            }
            .scrollClipDisabled()
            .focusSection()
        }
        if let season = detail.seasons.first(where: { $0.number == seasonNumber }) ?? detail.seasons.first {
            Shelf(title: season.name) {
                ForEach(season.episodes) { EpisodeCard(episode: $0) }
            }
        }
    }

    /// First unfinished episode in order (specials last), else the first one.
    private func upNext(_ detail: ShowDetail) -> EpisodeSummary? {
        let ordered = detail.seasons.filter { $0.number > 0 } + detail.seasons.filter { $0.number == 0 }
        let all = ordered.flatMap(\.episodes)
        return all.first { !$0.isCompleted } ?? all.first
    }
}

struct EpisodeCard: View {
    let episode: EpisodeSummary
    @Environment(AppModel.self) private var app
    @Environment(PlayerPresenter.self) private var player

    var body: some View {
        Button {
            player.play(episode.id, app: app)
        } label: {
            CardLabel(
                url: app.client?.mediaImage(episode.id),
                size: CardSize.landscape,
                title: "\(episode.episodeNumber). \(episode.title)",
                subtitle: runtime(episode.runtimeMinutes),
                progress: episode.progress,
                watched: episode.isCompleted
            )
        }
        .buttonStyle(.borderless)
        .contextMenu {
            if episode.positionSecs > 0 && !episode.isCompleted {
                Button("Start Over", systemImage: "arrow.counterclockwise") {
                    player.play(episode.id, app: app, from: 0)
                }
            }
            Button(episode.isCompleted ? "Mark as Unwatched" : "Mark as Watched",
                   systemImage: episode.isCompleted ? "circle" : "checkmark") {
                Task {
                    try? await app.client?.setWatched(episode.id, !episode.isCompleted)
                    app.watchStateChanged()
                }
            }
        }
    }
}
