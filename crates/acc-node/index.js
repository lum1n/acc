'use strict'

const native = require(`./acc.${process.platform}-${process.arch}.node`)

/** Moves the `[Kind]` the native side puts first in a message to `code`. */
function coded(err) {
  if (err instanceof Error) {
    const m = /^\[([A-Za-z]+)\] /.exec(err.message)
    if (m) {
      err.code = m[1]
      err.message = err.message.slice(m[0].length)
    }
  }
  return err
}

function wrap(fn) {
  return function (...args) {
    let out
    try {
      out = fn.apply(this, args)
    } catch (err) {
      throw coded(err)
    }
    return out instanceof Promise ? out.catch((err) => Promise.reject(coded(err))) : out
  }
}

for (const cls of [native.SshHost, native.RemoteSession]) {
  for (const name of Object.getOwnPropertyNames(cls.prototype)) {
    const d = Object.getOwnPropertyDescriptor(cls.prototype, name)
    if (name !== 'constructor' && typeof d.value === 'function') {
      Object.defineProperty(cls.prototype, name, { ...d, value: wrap(d.value) })
    }
  }
}

class SshHost extends native.SshHost {
  constructor(config) {
    try {
      super(config)
    } catch (err) {
      throw coded(err)
    }
  }
}

module.exports = {
  SshHost,
  RemoteSession: native.RemoteSession,
  generateKey: wrap(native.generateKey),
  newSessionId: wrap(native.newSessionId),
  harnesses: native.harnesses,
}
