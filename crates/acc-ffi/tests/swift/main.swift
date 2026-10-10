// Drives a recorded Claude Code session through the real library from Swift.
// Top-level code needs the file to be named main.swift. Built and run on macOS by CI (see .github/workflows/ci.yml, job `ios`):
//
//   swiftc main.swift acc_ffi.swift -I bindings/swift \
//     -Xcc -fmodule-map-file=bindings/swift/acc_ffiFFI.modulemap \
//     -L target/release -lacc_ffi -o smoke && ./smoke <repo root>

import Foundation

func check(_ ok: Bool, _ what: String) {
    if !ok {
        print("FAIL: \(what)")
        exit(1)
    }
}

let root = URL(fileURLWithPath: CommandLine.arguments[1])
let fixture = try! Data(contentsOf: root.appendingPathComponent(
    "crates/acc-core/tests/fixtures/claude/permission-deny.ndjson"))

check(!accVersion().isEmpty, "version")
check(harnesses().count == 6, "six harnesses")

let session = try! Session(config: SessionConfig(harness: .claude, cwd: "/home/v/proj", opening: .new))
check(session.launchSpec().program == "claude", "launch program")
check(String(decoding: session.start(), as: UTF8.self).contains("initialize"), "handshake")

var now: UInt64 = 1_000
var answered = false
var sawDelta = false
var sawTurnEnd = false
var offset = 0
while offset < fixture.count {
    let end = min(fixture.count, offset + 997)
    now += 1
    let got = session.receive(bytes: fixture.subdata(in: offset..<end), nowMs: now)
    offset = end
    for event in got.events {
        switch event {
        case let .permissionRequested(requestId, _, options) where !answered:
            let deny = options.first { $0.id.lowercased().contains("deny") || $0.id.lowercased().contains("reject") }!
            let bytes = try! session.send(command: .answerPermission(requestId: requestId, optionId: deny.id))
            check(String(decoding: bytes, as: UTF8.self).contains(requestId), "answer carries request id")
            answered = true
        case .itemDelta:
            sawDelta = true
        case .turnCompleted:
            sawTurnEnd = true
        default:
            break
        }
    }
}

check(answered && sawDelta && sawTurnEnd, "permission, text and turn end")
check(session.nativeId() == "5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11", "native id")
check(!session.commands().isEmpty, "command palette")
check(session.state()?.source == .native, "native state")

do {
    _ = try session.send(command: .answerPermission(requestId: "nope", optionId: "x"))
    check(false, "expected an error")
} catch AccError.UnknownRequest(let id) {
    check(id == "nope", "error carries request id")
} catch {
    check(false, "unexpected error \(error)")
}

let status = Osc7501Scanner().feed(bytes: Data("\u{1b}]7501;state=working:progress=40\u{07}".utf8))
check(status.first?.state == .working && status.first?.progress == 40, "OSC 7501")

// The async SSH surface: a refused connection and a rejected input both
// come back as typed errors. Nothing listens on port 1 of a CI runner.
let host = try! SshHost(config: SshConfig(
    host: "127.0.0.1", port: 1, user: "nobody", auth: .password(password: "x"),
    hostKey: .acceptAny, connectTimeoutMs: 2_000))
do {
    try await host.connect()
    check(false, "expected a connection error")
} catch AccError.Ssh, AccError.Disconnected, AccError.Timeout {
} catch {
    check(false, "unexpected error \(error)")
}
check(host.connections() == 0 && host.hostKeyFingerprint() == nil, "no connection")

let pane = host.paneSession(pane: PaneRef(multiplexer: .tmux, server: "", session: nil, pane: "%0"), harness: .claude)
check(pane.pane().pane == "%0" && pane.state() == nil, "pane session")
do {
    try await pane.sendInput(inputs: [.keys(keys: [.ctrl(letter: "12")])])
    check(false, "expected an invalid argument")
} catch AccError.InvalidArgument {
} catch {
    check(false, "unexpected error \(error)")
}
try! await pane.stopWatching()
await host.disconnect()

print("ok: Swift drove a recorded Claude session through acc-ffi")
