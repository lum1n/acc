# ACC: Agent Control Core

One embeddable Rust core that **controls, observes and configures** the major CLI agent harnesses:
Claude Code, Codex, Cursor Agent, Copilot CLI, pi and opencode.

It ships as a native library for iOS and Android (UniFFI → Swift/Kotlin), the browser and Node
(wasm-bindgen), and desktop (plain Rust). It needs **nothing installed on the hosts** it controls:
everything runs over an SSH exec channel using the harnesses' own headless protocols and standard
POSIX tools.

> Status: drivers for all six harnesses (Claude Code stream-json, Codex app-server, ACP for Cursor,
> Copilot and opencode, pi RPC), detached sessions with replay and multi-client attach, discovery
> and takeover of TUIs ACC didn't start, and the Swift/Kotlin surface for all of it. Used by
> [Sessh](https://github.com/lum1n/sessh-swift) to drive agents natively from iOS.

## Workspace

| Crate | What it is |
|---|---|
| `crates/acc-core` | Sans-IO core: harness catalogue, drivers, event stream, commands/options, sessions (launch, attach with replay, transcript, permissions), typed items and diffs, discovery, observation and takeover of foreign TUIs, detached-session and fan-in scripts, multiplexers |
| `crates/acc-ffi` | UniFFI bindings for Swift and Kotlin (`staticlib` + `cdylib`): `AccSession`, `AccObserved`, `AccTakeover`, `AccFanIn` and the host scripts, as a byte pump with no callbacks |
| `crates/acc-wasm` | wasm-bindgen bindings for browsers and Node |
| `crates/acc-cli` | `acc` command-line tool for hosts and spikes |

## Quick start

```sh
cargo test --workspace

# Which harnesses are installed here?
cargo run -p acc-cli -- harnesses

# Start Claude Code detached on a host, nothing installed there:
cargo run -q -p acc-cli -- script launch claude s1 ~/project | ssh host sh
cargo run -q -p acc-cli -- script attach s1 | ssh host sh      # follow output
cargo run -q -p acc-cli -- script status s1 | ssh host sh

# Swift package for iOS (macOS + Xcode): bindings/swift with AccFFI.xcframework
scripts/build-xcframework.sh

# Swift / Kotlin bindings
cargo build -p acc-ffi --release
cargo run -p acc-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/release/libacc_ffi.so --language swift --language kotlin --out-dir bindings
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the decisions behind it.

## Driving a session from an app

The FFI is a byte pump: the app owns SSH and runs the scripts ACC hands it.

```swift
let session = try AccSession.launch(request: req, id: newSessionId(harness: .claude, entropy: random15),
                                    clientTag: "phone", nowMs: now)
_ = try ssh.run(session.takeLaunchExec()!)               // starts the harness detached, prints its PID
let feed = AccFanIn(entries: [AccFeedEntry(key: id, path: .sessionLog(id: id), offset: 0)])
ssh.stream(feed.exec()) { bytes in                        // one channel follows every session
  for line in feed.feed(bytes: bytes) {
    for update in session.feed(bytes: line.bytes, nowMs: now) { render(update) }
    let out = session.takeTransmit()                      // handshakes, replies the protocol requires
    if !out.isEmpty { ssh.run(session.inputExec(bytes: out)) }
  }
}
switch try session.command(command: .prompt(text: "fix the tests")) {
case .send(let bytes): ssh.run(session.inputExec(bytes: bytes))
case .relaunch(let request): /* e.g. Claude fork: start `request` as a new session */
}
```

Any client can join later with `AccSession.attach(specJson:clientTag:)` (specs come from
`listSessionsExec()`): the log is replayed through a fresh driver without re-answering anything,
then the session is live. `discoverExec()` finds harness TUIs a user started themselves;
`AccObserved` follows them from their transcripts and `AccTakeover` closes one safely and resumes it
headless.

Apps that already ship a Rust static library should link `acc-ffi` into it (one `staticlib` crate
depending on both) rather than adding a second Rust library to the binary.
