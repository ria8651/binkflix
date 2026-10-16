import SwiftUI

struct ServerSetupView: View {
    @Environment(AppModel.self) private var app
    @State private var address = ""
    @State private var connecting = false
    @State private var error: String?

    var body: some View {
        VStack(spacing: 48) {
            Text("binkflix")
                .font(.system(size: 96, weight: .heavy))
            Text("Enter your server's address")
                .font(.title3)
                .foregroundStyle(.secondary)
            TextField("https://binkflix.example.com", text: $address)
                .textContentType(.URL)
                .keyboardType(.URL)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .frame(width: 900)
                .onSubmit(connect)
            Button(connecting ? "Connecting…" : "Connect", action: connect)
                .disabled(connecting || address.trimmingCharacters(in: .whitespaces).isEmpty)
            if let error {
                Text(error)
                    .foregroundStyle(.red)
                    .frame(maxWidth: 1200)
            }
        }
        .padding(80)
    }

    private func connect() {
        guard let url = Self.normalize(address) else {
            error = "That doesn't look like an address."
            return
        }
        connecting = true
        error = nil
        Task {
            defer { connecting = false }
            do {
                try await app.connect(to: url)
            } catch {
                self.error = "Couldn't reach \(url.absoluteString): \(error.localizedDescription)"
            }
        }
    }

    /// Accepts `host:port` as well as full URLs; assumes http for bare hosts.
    static func normalize(_ input: String) -> URL? {
        var s = input.trimmingCharacters(in: .whitespacesAndNewlines)
        while s.hasSuffix("/") { s.removeLast() }
        if !s.contains("://") { s = "http://" + s }
        guard let url = URL(string: s), url.host() != nil else { return nil }
        return url
    }
}
