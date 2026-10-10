// Drives a recorded Claude Code session through the real library from Kotlin
// on the JVM, with the same generated bindings Android uses. Run by CI (job
// `android`, see .github/workflows/ci.yml):
//
//   kotlinc smoke.kt bindings/kotlin/uniffi/acc_ffi/acc_ffi.kt -cp jna.jar:coroutines.jar -include-runtime -d smoke.jar
//   java -Djna.library.path=target/release -cp smoke.jar:jna.jar:coroutines.jar SmokeKt <repo root>

import java.io.File
import java.net.ServerSocket
import kotlin.system.exitProcess
import kotlinx.coroutines.runBlocking
import uniffi.acc_ffi.*

fun check(ok: Boolean, what: String) {
    if (!ok) {
        println("FAIL: $what")
        exitProcess(1)
    }
}

fun main(args: Array<String>) {
    val fixture = File(args[0], "crates/acc-core/tests/fixtures/claude/permission-deny.ndjson").readBytes()

    check(accVersion().isNotEmpty(), "version")
    check(harnesses().size == 6, "six harnesses")

    val session = Session(SessionConfig(Harness.CLAUDE, "/home/v/proj", Opening.New))
    check(session.launchSpec().program == "claude", "launch program")
    check(String(session.start()).contains("initialize"), "handshake")

    var now = 1_000UL
    var answered = false
    var sawDelta = false
    var sawTurnEnd = false
    var offset = 0
    while (offset < fixture.size) {
        val end = minOf(fixture.size, offset + 997)
        now += 1UL
        val got = session.receive(fixture.copyOfRange(offset, end), now)
        offset = end
        for (event in got.events) {
            when (event) {
                is Event.PermissionRequested -> if (!answered) {
                    val deny = event.options.first { it.id.lowercase().let { id -> "deny" in id || "reject" in id } }
                    val bytes = session.send(Command.AnswerPermission(event.requestId, deny.id))
                    check(String(bytes).contains(event.requestId), "answer carries request id")
                    answered = true
                }
                is Event.ItemDelta -> sawDelta = true
                is Event.TurnCompleted -> sawTurnEnd = true
                else -> {}
            }
        }
    }

    check(answered && sawDelta && sawTurnEnd, "permission, text and turn end")
    check(session.nativeId() == "5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11", "native id")
    check(session.commands().isNotEmpty(), "command palette")
    check(session.state()?.source == StateSource.NATIVE, "native state")

    try {
        session.send(Command.AnswerPermission("nope", "x"))
        check(false, "expected an error")
    } catch (e: AccException.UnknownRequest) {
        check(e.requestId == "nope", "error carries request id")
    }

    val status = Osc7501Scanner().feed("\u001b]7501;state=working:progress=40\u0007".toByteArray())
    check(status.firstOrNull()?.state == ProgramState.WORKING && status.firstOrNull()?.progress == 40.toUByte(), "OSC 7501")

    session.close()
    runBlocking { sshCalls() }
    println("ok: Kotlin drove a recorded Claude session through acc-ffi")
}

// The suspend SSH surface: a refused connection and a rejected input both
// come back as typed exceptions.
suspend fun sshCalls() {
    val closedPort = ServerSocket(0).use { it.localPort }
    val host = SshHost(SshConfig("127.0.0.1", closedPort.toUShort(), "nobody", SshAuth.Password("x"), HostKeyCheck.AcceptAny, 2_000UL))
    try {
        host.connect()
        check(false, "expected a connection error")
    } catch (e: AccException.Ssh) {
    } catch (e: AccException.Disconnected) {
    } catch (e: AccException.Timeout) {
    }
    check(host.connections() == 0UL && host.hostKeyFingerprint() == null, "no connection")

    val pane = host.paneSession(PaneRef(Multiplexer.TMUX, "", null, "%0"), Harness.CLAUDE)
    check(pane.pane().pane == "%0" && pane.state() == null, "pane session")
    try {
        pane.sendInput(listOf(PaneInput.Keys(listOf(PaneKey.Ctrl("12")))))
        check(false, "expected an invalid argument")
    } catch (e: AccException.InvalidArgument) {
    }
    pane.stopWatching()
    host.disconnect()
}
