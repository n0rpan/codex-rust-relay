// Run on the native macOS runner against an extracted OX Desktop.app.
import AppKit
import CoreGraphics
import Foundation
import Metal
import Darwin

var fixtureHost: Process?

func fail(_ message: String) -> Never {
    if let host = fixtureHost, host.isRunning { host.terminate(); host.waitUntilExit() }
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

guard CommandLine.arguments.count == 2 else { fail("Expected a path to OX Desktop.app") }
let bundle = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
let contents = bundle.appendingPathComponent("Contents/MacOS")
let companion = Process()
companion.executableURL = contents.appendingPathComponent("ox")
companion.arguments = ["--version"]
try companion.run()
companion.waitUntilExit()
guard companion.terminationStatus == 0 else { fail("Packaged ox failed to launch") }

// A hosted VM may lack a logged-in graphical session or a Metal-capable device.
// Still verify the archive, signature and companion there. Report what actually ran.
guard CGSessionCopyCurrentDictionary() != nil,
      CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) != nil else {
    print("::warning::WindowServer unavailable. Verified the native bundle and CLI. GUI smoke requires a graphical macOS host.")
    exit(0)
}
guard MTLCreateSystemDefaultDevice() != nil else {
    print("::warning::Metal device unavailable. Verified the native bundle and CLI. GUI smoke requires a Metal-capable macOS host.")
    exit(0)
}

let files = FileManager.default
let fixture = files.temporaryDirectory.appendingPathComponent("ox-desktop-\(UUID().uuidString)")
let home = fixture.appendingPathComponent("home")
let project = fixture.appendingPathComponent("project")
for directory in [home, project] {
    try files.createDirectory(at: directory, withIntermediateDirectories: true)
}
defer { try? files.removeItem(at: fixture) }

// Exercise the same LaunchServices entry point used by Finder.
let config = NSWorkspace.OpenConfiguration()
config.arguments = [project.path]
// AppKit and IconServices share the logged-in user's home over XPC.
// Isolate the coding clients without redirecting native macOS services.
let host = Process()
let socketDirectory = URL(fileURLWithPath: "/tmp/ox-smoke-\(UUID().uuidString)")
try files.createDirectory(at: socketDirectory, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
defer { try? files.removeItem(at: socketDirectory) }
let hostSocket = socketDirectory.appendingPathComponent("host.sock").path
let hostState = fixture.appendingPathComponent("host-state").path
let fixtureEnvironment = [
    "OX_HOST_SOCKET": hostSocket,
    "OX_HOST_STATE_DIR": hostState,
    "XDG_STATE_HOME": fixture.appendingPathComponent("state").path,
    "CODEX_HOME": home.appendingPathComponent(".codex").path,
    "CLAUDE_CONFIG_DIR": home.appendingPathComponent(".claude").path
]
config.environment = fixtureEnvironment
host.executableURL = bundle.appendingPathComponent("Contents/MacOS/ox-host")
host.arguments = ["run", "--socket", hostSocket, "--state-dir", hostState]
host.environment = ProcessInfo.processInfo.environment.merging(fixtureEnvironment) { _, want in want }
try host.run()
fixtureHost = host
defer { if host.isRunning { host.terminate(); host.waitUntilExit() } }
let hostDeadline = Date().addingTimeInterval(12)
while !files.fileExists(atPath: hostSocket) && host.isRunning && Date() < hostDeadline {
    Thread.sleep(forTimeInterval: 0.02)
}
guard files.fileExists(atPath: hostSocket) else { fail("The packaged session host did not open its socket") }
config.createsNewApplicationInstance = true
config.activates = true
var application: NSRunningApplication?
var launchError: Error?
NSWorkspace.shared.openApplication(at: bundle, configuration: config) { app, error in
    application = app
    launchError = error
}
let launchDeadline = Date().addingTimeInterval(60)
while application == nil && launchError == nil && Date() < launchDeadline {
    RunLoop.current.run(until: Date().addingTimeInterval(0.1))
}
guard let app = application else {
    fail("LaunchServices could not open OX Desktop: \(String(describing: launchError))")
}
defer {
    if !app.isTerminated { app.terminate() }
}

func ownedWindows(_ options: CGWindowListOption) -> [[String: Any]] {
    let windows = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] ?? []
    return windows.filter { ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == app.processIdentifier }
}

func hasProjectWindow() -> Bool {
    ownedWindows(.optionOnScreenOnly).contains { window in
        guard let layer = window[kCGWindowLayer as String] as? NSNumber, layer.intValue == 0,
              let bounds = window[kCGWindowBounds as String] as? [String: NSNumber],
              let width = bounds["Width"], let height = bounds["Height"] else { return false }
        return width.intValue >= 700 && height.intValue >= 450
    }
}

let deadline = Date().addingTimeInterval(60)
while !app.isTerminated && Date() < deadline && !hasProjectWindow() {
    RunLoop.current.run(until: Date().addingTimeInterval(0.25))
}
if !hasProjectWindow() {
    let session = CGSessionCopyCurrentDictionary() as? [String: Any] ?? [:]
    print("LaunchServices PID \(app.processIdentifier), active=\(app.isActive), finishedLaunching=\(app.isFinishedLaunching), terminated=\(app.isTerminated)")
    print("Screens: \(NSScreen.screens.map { NSStringFromRect($0.frame) })")
    print("Frontmost app: \(NSWorkspace.shared.frontmostApplication?.bundleIdentifier ?? "unknown")")
    print("On console: \(String(describing: session["kCGSSessionOnConsoleKey"])), locked: \(String(describing: session["CGSSessionScreenIsLocked"]))")
    for window in ownedWindows(.optionAll) {
        print("Window: layer=\(String(describing: window[kCGWindowLayer as String])), bounds=\(String(describing: window[kCGWindowBounds as String])), onScreen=\(String(describing: window[kCGWindowIsOnscreen as String]))")
    }
    if !app.isTerminated {
        let report = fixture.appendingPathComponent("sample.txt")
        let sample = Process()
        sample.executableURL = URL(fileURLWithPath: "/usr/bin/sample")
        sample.arguments = [String(app.processIdentifier), "1", "-file", report.path]
        try? sample.run()
        if sample.isRunning { sample.waitUntilExit() }
        if let stack = try? String(contentsOf: report, encoding: .utf8),
           let graph = stack.range(of: "Call graph:") {
            print(stack[graph.lowerBound...].split(separator: "\n").prefix(55).joined(separator: "\n"))
        }
        app.terminate()
    }
    fail("LaunchServices opened the bundle, but WindowServer did not report a visible project window.")
}
print("Packaged OX Desktop mapped its project window on macOS.")
