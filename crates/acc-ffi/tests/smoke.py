"""Drives a recorded Claude Code session through the real UniFFI library.

Run after generating the Python bindings next to the built library:

    cargo build -p acc-ffi --release
    cargo run -p acc-ffi --features bindgen --bin uniffi-bindgen -- generate \
        --library target/release/libacc_ffi.so --language python --out-dir bindings/python
    cp target/release/libacc_ffi.so bindings/python/
    python3 crates/acc-ffi/tests/smoke.py bindings/python

The Swift and Kotlin bindings are generated from the same library metadata,
so this exercises the exact FFI calls and type conversions they use.
"""

import asyncio
import pathlib
import socket
import sys

sys.path.insert(0, sys.argv[1])
import acc_ffi as acc  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parents[3]
FIXTURE = ROOT / "crates/acc-core/tests/fixtures/claude/permission-deny.ndjson"


def main() -> None:
    assert acc.acc_version()
    assert {h.harness for h in acc.harnesses()} == set(acc.Harness)

    session = acc.Session(
        acc.SessionConfig(
            harness=acc.Harness.CLAUDE,
            cwd="/home/v/proj",
            opening=acc.Opening.NEW(),
            model=None,
            client_tag=None,
        )
    )
    spec = session.launch_spec()
    assert spec.program == "claude" and spec.protocol == acc.Protocol.CLAUDE_STREAM_JSON
    assert b"initialize" in session.start()

    events = []
    answered = None
    now = 1_000
    for line in FIXTURE.read_bytes().splitlines(keepends=True):
        now += 1
        got = session.receive(line, now)
        events.extend(got.events)
        for e in got.events:
            if e.is_PERMISSION_REQUESTED() and answered is None:
                deny = next(o for o in e.options if "deny" in o.id.lower() or "reject" in o.id.lower())
                answered = session.send(
                    acc.Command.ANSWER_PERMISSION(request_id=e.request_id, option_id=deny.id)
                )
                assert e.request_id.encode() in answered

    kinds = {type(e).__name__.removeprefix("Event.") for e in events}
    for kind in ("SESSION_STARTED", "STATE_CHANGED", "PERMISSION_REQUESTED", "ITEM_DELTA", "TURN_COMPLETED"):
        assert kind in kinds, f"no {kind} event in {sorted(kinds)}"
    assert answered is not None
    assert session.native_id() == "5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11"
    assert session.commands(), "palette is empty"
    assert any(o.id == acc.OptionId.PERMISSION for o in session.options())

    state = session.state()
    assert state is not None and state.source == acc.StateSource.NATIVE and state.observed_at_ms > 1_000

    try:
        session.send(acc.Command.ANSWER_PERMISSION(request_id="nope", option_id="x"))
        raise AssertionError("expected an error")
    except acc.AccError.UnknownRequest:
        pass

    detached = acc.DetachedSession("s1")
    assert "mkfifo" in detached.launch_script(spec, "/tmp")
    asyncio.run(ssh_calls())
    print(f"ok: {len(events)} events through the FFI")


async def ssh_calls() -> None:
    """The async SSH surface: a refused connection comes back as a typed
    error through the foreign future."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        closed_port = s.getsockname()[1]
    host = acc.SshHost(
        acc.SshConfig(
            host="127.0.0.1",
            port=closed_port,
            user="nobody",
            auth=acc.SshAuth.PASSWORD(password="x"),
            host_key=acc.HostKeyCheck.ACCEPT_ANY(),
            connect_timeout_ms=2_000,
            keepalive_interval_ms=None,
            keepalive_max=None,
        )
    )
    try:
        await host.connect()
        raise AssertionError("expected a connection error")
    except (acc.AccError.Ssh, acc.AccError.Disconnected, acc.AccError.Timeout):
        pass
    assert host.connections() == 0 and host.host_key_fingerprint() is None

    await host.disconnect()


main()
