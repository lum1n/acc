// Builds the native addon with cargo and copies it next to index.js.
import { execFileSync } from 'node:child_process'
import { copyFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const root = join(here, '..', '..', '..')
const release = process.argv.includes('--release')
execFileSync('cargo', ['build', '-p', 'acc-node', ...(release ? ['--release'] : [])], {
  cwd: root,
  stdio: 'inherit',
})
const lib = process.platform === 'darwin' ? 'libacc_node.dylib' : 'libacc_node.so'
const out = `acc.${process.platform}-${process.arch}.node`
copyFileSync(join(root, 'target', release ? 'release' : 'debug', lib), join(here, '..', out))
console.log(`built ${out}`)
