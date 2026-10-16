import CoreImage.CIFilterBuiltins
import SwiftUI
import UIKit

/// Device-flow pairing (see the server's `devices.rs`): show a code, let the
/// user approve it on `/link` from a signed-in browser, poll for the token.
struct PairingView: View {
    let client: APIClient

    @Environment(AppModel.self) private var app
    @State private var code: DeviceCode?
    @State private var error: String?

    var body: some View {
        HStack(spacing: 120) {
            VStack(alignment: .leading, spacing: 40) {
                Text("Link this Apple TV")
                    .font(.largeTitle.bold())
                if let code {
                    Text("On your phone or computer, go to")
                        .foregroundStyle(.secondary)
                    Text(linkURL(code.verificationUri).absoluteString)
                        .font(.title2.monospaced())
                    Text("and enter")
                        .foregroundStyle(.secondary)
                    Text(code.userCode)
                        .font(.system(size: 110, weight: .bold, design: .monospaced))
                        .tracking(8)
                } else if error == nil {
                    ProgressView()
                }
                if let error {
                    Text(error).foregroundStyle(.red)
                }
                Button("Use a different server") { app.forgetServer() }
            }
            .frame(maxWidth: 1000, alignment: .leading)

            if let code, let qr = qrImage(linkURL(code.verificationUriComplete)) {
                VStack(spacing: 24) {
                    Image(uiImage: qr)
                        .interpolation(.none)
                        .resizable()
                        .frame(width: 440, height: 440)
                        .padding(28)
                        .background(.white, in: .rect(cornerRadius: 24))
                    Text("Or scan to link")
                        .foregroundStyle(.secondary)
                }
            }
        }
        .padding(100)
        .task { await pair() }
    }

    private func linkURL(_ path: String) -> URL {
        URL(string: path, relativeTo: client.base)?.absoluteURL ?? client.base
    }

    /// Request a code and poll until approved; start over when a code expires.
    private func pair() async {
        while !Task.isCancelled {
            do {
                let name = UIDevice.current.name
                struct Start: Encodable { let name: String }
                let req = try client.request(client.url("/auth/device/code"), method: "POST", json: Start(name: name))
                let fresh: DeviceCode = try APIClient.decoder.decode(DeviceCode.self, from: await client.send(req))
                code = fresh
                error = nil
                if let token = try await poll(fresh) {
                    app.paired(token: token.token, login: token.login)
                    return
                }
            } catch is CancellationError {
                return
            } catch {
                self.error = error.localizedDescription
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    /// The token once approved, `nil` once the code expired.
    private func poll(_ code: DeviceCode) async throws -> DeviceToken? {
        struct Poll: Encodable { let device_code: String }
        let req = try client.request(client.url("/auth/device/token"), method: "POST", json: Poll(device_code: code.deviceCode))
        while true {
            try await Task.sleep(for: .seconds(max(code.interval, 1)))
            let (data, resp) = try await URLSession.shared.data(for: req)
            if (resp as? HTTPURLResponse)?.statusCode == 200 {
                return try APIClient.decoder.decode(DeviceToken.self, from: data)
            }
            struct Failure: Decodable { let error: String }
            let failure = try? APIClient.decoder.decode(Failure.self, from: data)
            if failure?.error != "authorization_pending" { return nil }
        }
    }

    private func qrImage(_ url: URL) -> UIImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(url.absoluteString.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage,
              let cg = CIContext().createCGImage(output, from: output.extent) else { return nil }
        return UIImage(cgImage: cg)
    }
}
