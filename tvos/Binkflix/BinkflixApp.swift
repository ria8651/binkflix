import SwiftUI

@main
struct BinkflixApp: App {
    @State private var app = AppModel()
    @State private var player = PlayerPresenter()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(app)
                .environment(player)
                .preferredColorScheme(.dark)
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var app

    var body: some View {
        switch app.phase {
        case .setup:
            ServerSetupView()
        case let .pairing(client):
            PairingView(client: client)
        case .ready:
            MainView()
        }
    }
}

struct MainView: View {
    @State private var homePath = NavigationPath()
    #if DEBUG
    @Environment(AppModel.self) private var app
    @Environment(PlayerPresenter.self) private var player
    #endif

    var body: some View {
        TabView {
            Tab("Home", systemImage: "house") {
                NavigationStack(path: $homePath) { HomeView().browseDestinations() }
            }
            Tab("Movies", systemImage: "film") {
                NavigationStack { MoviesView().browseDestinations() }
            }
            Tab("Shows", systemImage: "tv") {
                NavigationStack { ShowsView().browseDestinations() }
            }
            Tab(role: .search) {
                NavigationStack { SearchView().browseDestinations() }
            }
            Tab("Settings", systemImage: "gear") {
                NavigationStack { SettingsView() }
            }
        }
        #if DEBUG
        .task { await debugLaunch() }
        #endif
    }

    #if DEBUG
    /// Simulator testing without a remote: `-debugOpen movie:<id>|show:<id>`
    /// and `-debugPlay <id>` launch arguments.
    private func debugLaunch() async {
        let defaults = UserDefaults.standard
        if let open = defaults.string(forKey: "debugOpen") {
            let parts = open.split(separator: ":", maxSplits: 1).map(String.init)
            if parts.count == 2 {
                homePath.append(parts[0] == "show" ? Destination.show(parts[1]) : .movie(parts[1]))
            }
        }
        if let id = defaults.string(forKey: "debugPlay") {
            // Let any -debugOpen push settle first, or the player is presented
            // mid-transition and Menu goes to the page behind it.
            try? await Task.sleep(for: .seconds(3))
            player.play(id, app: app)
        }
    }
    #endif
}

/// Where a card leads.
enum Destination: Hashable {
    case movie(String)
    case show(String)
}

extension View {
    func browseDestinations() -> some View {
        navigationDestination(for: Destination.self) { dest in
            switch dest {
            case let .movie(id): MovieDetailView(id: id)
            case let .show(id): ShowDetailView(id: id)
            }
        }
    }
}
