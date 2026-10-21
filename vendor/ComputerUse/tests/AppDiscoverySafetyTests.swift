import AppKit
import Foundation

@main
enum AppDiscoverySafetyTests {
    static func main() throws {
        var checks = 0
        var failures: [String] = []
        func expect(_ condition: Bool, _ message: String) {
            checks += 1
            if !condition { failures.append(message) }
        }

        // Synthetic descriptors only; no application is launched or receives input.
        let application = NSRunningApplication.current
        func descriptor(_ identifier: String?, name: String = "Test App", pid: pid_t = 1) -> RunningAppDescriptor {
            RunningAppDescriptor(name: name, bundleIdentifier: identifier, pid: pid, runningApplication: application)
        }
        let blockedIdentifiers = [
            "com.1password.1password",
            "com.1password.safari",
            "com.bitwarden.desktop",
            "com.dashlane.dashlanephonefinal",
            "com.lastpass.LastPass",
            "com.nordsec.nordpass",
            "me.proton.pass.electron",
            "me.proton.pass.catalyst",
        ]
        let safe = descriptor("com.example.SafeApp", pid: 2)
        for identifier in blockedIdentifiers {
            for variant in [identifier, identifier.lowercased(), identifier.uppercased()] {
                expect(AppSafetyPolicy.isBlocked(bundleIdentifier: variant), "Policy allowed \(variant)")
                let blocked = descriptor(variant)
                expect(
                    AppDiscovery.resolvedRunningApp(in: [blocked], matching: identifier) == nil,
                    "Bundle identifier lookup allowed \(variant)"
                )
                expect(
                    AppDiscovery.resolvedRunningApp(in: [blocked], matching: "TEST APP") == nil,
                    "Name lookup allowed \(variant)"
                )
                expect(
                    AppDiscovery.resolvedRunningApp(in: [blocked, safe], matching: "Test App")?.pid == safe.pid,
                    "Blocked app displaced the safe name match for \(variant)"
                )
            }
        }

        for identifier in ["com.example.SafeApp", "COM.EXAMPLE.SAFEAPP", "com.lastpass.LastPass.helper", ""] {
            expect(!AppSafetyPolicy.isBlocked(bundleIdentifier: identifier), "Policy rejected safe identifier \(identifier)")
        }
        expect(!AppSafetyPolicy.isBlocked(bundleIdentifier: nil), "Policy rejected a missing identifier")
        expect(
            AppDiscovery.resolvedRunningApp(in: [safe], matching: "COM.EXAMPLE.SAFEAPP")?.pid == safe.pid,
            "Safe bundle identifier lookup must remain case insensitive"
        )
        expect(
            AppDiscovery.resolvedRunningApp(in: [safe], matching: "test app")?.pid == safe.pid,
            "Safe name lookup must remain case insensitive"
        )
        expect(
            AppDiscovery.resolvedRunningApp(in: [safe], matching: "com.example.Missing") == nil,
            "An unrelated bundle identifier must not match"
        )
        let unnamedBundle = descriptor(nil, name: "Unbundled", pid: 3)
        expect(
            AppDiscovery.resolvedRunningApp(in: [unnamedBundle], matching: "UNBUNDLED")?.pid == unnamedBundle.pid,
            "Name lookup for an app without a bundle identifier must remain supported"
        )

        guard failures.isEmpty else {
            for failure in failures { fputs("FAIL: \(failure)\n", stderr) }
            throw NSError(domain: "AppDiscoverySafetyTests", code: failures.count, userInfo: [
                NSLocalizedDescriptionKey: "\(failures.count) of \(checks) checks failed",
            ])
        }
        print("AppDiscovery safety tests passed (\(checks) checks)")
    }
}
