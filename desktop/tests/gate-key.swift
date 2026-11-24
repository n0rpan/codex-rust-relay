// Exercise the packaged app through the logged-in WindowServer session.
import AppKit
import CoreGraphics
import Darwin

guard CommandLine.arguments.count >= 2, let pid = Int32(CommandLine.arguments[1]),
      let app = NSRunningApplication(processIdentifier: pid) else { exit(2) }
guard CGPreflightPostEventAccess() else { exit(77) }
if !app.isActive {
    app.activate(options: [.activateAllWindows])
    let deadline = Date().addingTimeInterval(2)
    while NSWorkspace.shared.frontmostApplication?.processIdentifier != pid && Date() < deadline {
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))
    }
}
guard NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else {
    fputs("Could not focus the packaged project window.\n", stderr)
    exit(1)
}
let source = CGEventSource(stateID: .hidSystemState)
func key(_ code: CGKeyCode, _ flags: CGEventFlags = [], _ text: String? = nil) {
    for down in [true, false] {
        guard let event = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down) else { exit(1) }
        event.flags = flags
        if let text {
            let units = Array(text.utf16)
            event.keyboardSetUnicodeString(stringLength: units.count, unicodeString: units)
        }
        event.post(tap: .cghidEventTap)
        usleep(25_000)
    }
}
if CommandLine.arguments.count == 2 {
    key(125, [.maskCommand, .maskAlternate], "\u{F701}")
} else {
    // Select the named thread with the same command palette as a laptop user.
    key(40, .maskCommand)
    usleep(250_000)
    for c in CommandLine.arguments[2] { key(0, [], String(c)) }
    usleep(350_000)
    key(36)
}
