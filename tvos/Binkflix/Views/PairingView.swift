import CoreImage.CIFilterBuiltins
import SwiftUI
import UIKit

/// Bastion's device sign-in: show a code, let the user confirm it on
/// bastion's `/device` page from their phone, poll for the tokens.
struct PairingView: View {
    let session: BastionSession

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
                    Text(code.verificationUri)
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

            if let code, let url = URL(string: code.verificationUriComplete), let qr = qrImage(url) {
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

    /// Request a code and poll until approved; start over when a code
    /// expires or is denied.
    private func pair() async {
        while !Task.isCancelled {
            do {
                let fresh = try await session.startPairing(name: UIDevice.current.name)
                code = fresh
                error = nil
                if let tokens = try await poll(fresh) {
                    try await app.paired(session, tokens)
                    return
                }
            } catch is CancellationError {
                return
            } catch let e as OAuthError where e.error == "invalid_client" {
                self.error = "Device sign-in is off for \(session.info.service). Turn it on from the service's page in bastion."
                code = nil
                try? await Task.sleep(for: .seconds(10))
            } catch {
                self.error = error.localizedDescription
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    /// The tokens once approved, `nil` once the code expired or was denied.
    private func poll(_ code: DeviceCode) async throws -> BastionTokens? {
        var interval = max(code.interval, 1)
        let deadline = ContinuousClock.now + .seconds(code.expiresIn)
        while ContinuousClock.now < deadline {
            try await Task.sleep(for: .seconds(interval))
            do {
                return try await session.poll(code)
            } catch let e as OAuthError {
                switch e.error {
                case "authorization_pending": continue
                case "slow_down": interval += 5
                case "access_denied":
                    error = "That code was declined."
                    return nil
                case "expired_token": return nil
                default: throw e
                }
            }
        }
        return nil
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
