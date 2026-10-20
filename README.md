# ACC: Agent Control Core

One embeddable Rust core that **controls, observes and configures** the major CLI agent harnesses:
Claude Code, Codex, Cursor Agent, Copilot CLI, pi and opencode.

It ships as a native library for iOS and Android (UniFFI → Swift/Kotlin), the browser and Node
(wasm-bindgen), Node servers with the SSH transport (napi), and desktop (plain Rust). It needs **nothing installed on the hosts** it controls:
everything runs over an SSH exec channel using the harnesses' own headless protocols and standard
POSIX tools.

> Status: drivers for all six harnesses (Claude Code stream-json, Codex app-server, ACP for Cursor,
> Copilot and opencode, pi RPC), detached sessions with replay and multi-client attach, and the
> Swift/Kotlin surface for all of it. Used by [Sessh](https://github.com/lum1n/sessh-swift) to
> drive agents natively from iOS.

ACC only knows the sessions it starts. Harness TUIs a user started themselves (in a terminal, or a
tmux, zellij, screen or Rex pane) are the job of observer, a separate library in its own
repository, built on ACC: it finds them, follows them, co-drives their panes and takes them over
into ordinary ACC sessions.

## Workspace

| Crate | What it is |
|---|---|
| `crates/acc-core` | Sans-IO core: harness catalogue, drivers, event stream, commands/options, live sessions (`live`) and detached sessions (`session`: launch, attach with replay, transcript, permissions, snapshots), typed items and diffs, detached-session and fan-in scripts, and across harnesses: one permission policy, a git worktree per session, handoff to another harness, usage added up per account and day, skills and MCP servers in every harness's own places, and host-side events while no client is connected |
| `crates/acc-ssh` | Daemonless SSH transport (pure-Rust russh): run any driver on a host over plain SSH, surviving dropped connections |
| `crates/acc-ffi` | UniFFI bindings for Swift and Kotlin (`staticlib` + `cdylib`): the live `Session`, SSH sessions, and the detached-session byte pump (`AccSession`, `AccFanIn` and the host scripts) |
| `crates/acc-wasm` | wasm-bindgen bindings for browsers and Node: the same objects and functions as `acc-ffi` except the SSH transport (a test keeps them in step), records as plain JS objects in the core's serde shapes, bytes as `Uint8Array` |
| `crates/acc-node` | napi bindings for Node servers: the SSH transport included (`SshHost`, `RemoteSession`), plus `generateKey` for a key of the server's own. Values in the core's serde shapes, as acc-wasm passes them; released as `acc-node-<version>-<platform>.tar.gz` |
| `crates/acc-cli` | `acc` command-line tool for hosts and spikes |

## Quick start

```sh
cargo test --workspace

# What can ACC drive on this machine?
cargo run -q -p acc-cli -- doctor

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

# JS bindings (browser: --target web)
cargo build -p acc-wasm --release --target wasm32-unknown-unknown
wasm-bindgen --target nodejs --out-dir bindings/node target/wasm32-unknown-unknown/release/acc_wasm.wasm
```

From Swift, a session is a few calls; the app moves the bytes (see `docs/ARCHITECTURE.md`, Bindings):

```swift
let session = try Session(config: SessionConfig(harness: .claude, cwd: "/home/me/project", opening: .new))
channel.exec(session.launchSpec())            // your SSH channel or pipe
channel.write(session.start())
channel.onData { bytes in
    let got = session.receive(bytes: bytes, nowMs: nowMs())
    channel.write(got.transmit)
    for event in got.events { ui.apply(event) }
}
channel.write(try session.send(command: .prompt(text: "hi")))
```

Or let the library do the SSH too (pure Rust, nothing installed on the host). Sessions survive the
phone dropping off the network or being suspended, and any client can rejoin them:

```swift
let host = try SshHost(config: SshConfig(host: "devbox", port: 22, user: "me",
    auth: .privateKey(key: pem, passphrase: nil), hostKey: .fingerprints(fingerprints: [pinned])))
let session = try await host.launchSession(id: "s1", config: SessionConfig(
    harness: .claude, cwd: "/home/me/project", opening: .new))
try await session.send(command: .prompt(text: "hi"))
while true { for event in try await session.nextEvents() { ui.apply(event) } }
```

## Try it by hand

Everything below runs in a plain terminal. Install the CLI once:

```sh
cargo install --path crates/acc-cli    # puts `acc` in ~/.cargo/bin
acc doctor                             # harnesses found here and their versions
```

**Drive a harness.** `acc run <harness> [dir]` starts it headlessly through its ACC driver
(`claude`, `codex`, `pi`, `cursor`, `copilot`, `opencode`). Type prompts; events print as
readable lines (`--json` prints the raw event stream). `:help` lists the commands: `:answer allow`
for a permission prompt, `:interrupt`, `:compact`, `:set model <m>`, `:commands`, `:options`.
Ctrl-D ends the session after the current turn.

```sh
acc run claude ~/project
echo "Reply with: hello from acc" | acc run claude .    # one-shot smoke test
acc run codex . --model gpt-5.5 --json
```

**On another machine, over SSH.** Add `--ssh [user@]host[:port]` and the harness runs detached
on that host, with nothing installed there. Ending input (Ctrl-D) or losing the connection leaves
it running; `--attach` rejoins it from any machine and replays what you missed. Keys come from
the running ssh-agent (`SSH_AUTH_SOCK`), else `~/.ssh`; `--identity` names a key file. Host keys
are checked against `~/.ssh/known_hosts`.

```sh
acc run claude ~/project --ssh me@devbox            # prints the id to rejoin with
acc ssh-sessions me@devbox                          # detached sessions on that host
acc run claude --ssh me@devbox --attach <id>        # rejoin, from the laptop or anywhere
acc ssh-sessions me@devbox --stop <id> --prune      # stop one, clean up ended ones
```

**Resume a session.** `--resume <id>` continues a harness's own session headlessly (Claude
resumes per project, so run it from the session's directory). Quit the harness's TUI first if it
has the session open; observer lists the sessions on a machine and takes over a
running TUI for you.

```sh
acc run claude . --resume <session-id>
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
switch try session.command(command: .prompt(text: "fix the tests", attachments: [])) {
case .send(let bytes): ssh.run(session.inputExec(bytes: bytes))
case .relaunch(let request): /* e.g. Claude fork: start `request` as a new session */
}
```

Any client can join later with `AccSession.attach(specJson:clientTag:)` (specs come from
`listSessionsExec()`): the log is replayed through a fresh driver without re-answering anything,
then the session is live. Persist `session.snapshot()` and pass it to
`AccSession.restore(snapshot:specJson:)` after a relaunch to replay only what the log gained since.
`LaunchRequest.labels` keeps notes of the app's own with the session (where it came from, say);
they are stored in `spec.json` and carried over to relaunches.

Apps that already ship a Rust static library should link `acc-ffi` into it (one `staticlib` crate
depending on both) rather than adding a second Rust library to the binary.

## License

MIT. See [LICENSE](LICENSE).
