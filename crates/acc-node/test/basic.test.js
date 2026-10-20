'use strict'

const test = require('node:test')
const assert = require('node:assert')
const { spawn, execFileSync } = require('node:child_process')
const { existsSync, mkdtempSync, rmSync, writeFileSync } = require('node:fs')
const net = require('node:net')
const os = require('node:os')
const { join } = require('node:path')
const { SshHost, generateKey, newSessionId, harnesses } = require('..')

const T = { timeout: 30_000 }

test('generates keys that load as a login', () => {
  const key = generateKey('nudge@test')
  assert.match(key.publicKey, /^ssh-ed25519 \S+ nudge@test$/)
  assert.match(key.privateKey, /BEGIN OPENSSH PRIVATE KEY/)
  new SshHost({ host: 'h', user: 'u', auth: { privateKey: key.privateKey }, hostKey: { acceptAny: true } })
})

test('makes session ids and lists the harnesses', () => {
  assert.match(newSessionId('claude', Buffer.alloc(15, 7)), /^claude-[a-z2-7]{24}$/)
  assert.deepStrictEqual(
    harnesses().map((h) => h.harness),
    ['claude', 'codex', 'cursor', 'copilot', 'pi', 'opencode'],
  )
})

test('errors carry the kind as code', () => {
  assert.throws(
    () => new SshHost({ host: 'h', user: 'u', auth: {}, hostKey: { acceptAny: true } }),
    (e) => e.code === 'InvalidArgument' && /exactly one/.test(e.message),
  )
  assert.throws(
    () => newSessionId('emacs', Buffer.alloc(15)),
    (e) => e.code === 'InvalidArgument',
  )
})

/** A throwaway sshd on localhost that lets `publicKey` in, as acc-ssh's tests run one. */
async function sshd(t, publicKey) {
  const bin = ['/usr/sbin/sshd', '/usr/bin/sshd', '/usr/local/sbin/sshd', '/opt/homebrew/sbin/sshd'].find(existsSync)
  if (!bin) {
    if (process.env.ACC_REQUIRE_SSHD) throw new Error('sshd is required')
    return null
  }
  const dir = mkdtempSync(join(os.tmpdir(), 'acc-node-sshd-'))
  execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-f', join(dir, 'host')])
  writeFileSync(join(dir, 'authorized_keys'), publicKey + '\n')
  const fingerprint = execFileSync('ssh-keygen', ['-lf', join(dir, 'host.pub')]).toString().split(/\s+/)[1]
  const port = await new Promise((resolve) => {
    const s = net.createServer().listen(0, '127.0.0.1', () => {
      const { port } = s.address()
      s.close(() => resolve(port))
    })
  })
  writeFileSync(
    join(dir, 'sshd_config'),
    `Port ${port}\nListenAddress 127.0.0.1\nHostKey ${dir}/host\nAuthorizedKeysFile ${dir}/authorized_keys\n` +
      `PasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nPidFile ${dir}/pid\nLogLevel ERROR\n`,
  )
  const child = spawn(bin, ['-D', '-e', '-f', join(dir, 'sshd_config')], { stdio: 'ignore' })
  t.after(() => {
    child.kill()
    rmSync(dir, { recursive: true, force: true })
  })
  for (let i = 0; i < 100; i++) {
    const up = await new Promise((resolve) => {
      const c = net.connect(port, '127.0.0.1', () => (c.end(), resolve(true))).on('error', () => resolve(false))
    })
    if (up) break
    await new Promise((r) => setTimeout(r, 50))
  }
  return { port, fingerprint }
}

test('logs in with its own key, checks the host key, runs scripts and lists sessions', T, async (t) => {
  const key = generateKey('nudge@test')
  const server = await sshd(t, key.publicKey)
  if (!server) return t.skip('sshd not installed')
  const config = (hostKey) => ({
    host: '127.0.0.1',
    port: server.port,
    user: os.userInfo().username,
    auth: { privateKey: key.privateKey },
    hostKey,
    connectTimeoutMs: 5000,
  })

  // Trust on first use: connect accepting any key, then pin what it showed.
  const first = new SshHost(config({ acceptAny: true }))
  await first.connect()
  assert.strictEqual(first.hostKeyFingerprint(), server.fingerprint)
  await first.disconnect()

  const host = new SshHost(config({ fingerprints: [server.fingerprint] }))
  const out = await host.exec('read x; echo "got $x"; echo oops >&2; exit 3', Buffer.from('hi\n'))
  assert.strictEqual(out.stdout.toString(), 'got hi\n')
  assert.strictEqual(out.stderr.toString(), 'oops\n')
  assert.strictEqual(out.exitCode, 3)
  assert.ok(Array.isArray(await host.probe()))
  assert.ok(Array.isArray(await host.listSessions()))
  await host.disconnect()

  const wrong = new SshHost(config({ fingerprints: ['SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'] }))
  await assert.rejects(wrong.connect(), (e) => e.code === 'HostKeyRejected')
})
