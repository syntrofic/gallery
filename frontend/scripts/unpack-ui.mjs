import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const working = new URL('../vendor/', import.meta.url)
const archive = fileURLToPath(new URL('syntrofic-ui-9d28781.tar.gz', working))
const destination = fileURLToPath(new URL('ui/', working))
const digest = createHash('sha256').update(readFileSync(archive)).digest('hex')
if (digest !== '722b849a3075434c166f5bab4fb490e46b53f1357a395ddb81c1c844b4a17ff3') throw new Error('Syntrofic UI archive checksum mismatch')
mkdirSync(destination, { recursive: true })
const extracted = spawnSync('tar', ['-xzf', archive, '-C', destination], { stdio: 'inherit' })
if (extracted.error) throw extracted.error
if (extracted.status !== 0) process.exit(extracted.status || 1)
