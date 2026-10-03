// CI-only TAP guard for the Windows Node acceptance lane.
//
// Node 20's `--test-name-pattern` emits `# SKIP test name does not match pattern`
// for every non-matching test and counts it as skipped; Node 24 omits them. The
// selected Windows cases must never skip for any other reason, and the previous
// `# skipped 0` total was therefore unsatisfiable on the pinned Node 20 engine.
import { readFileSync } from "node:fs"
import { pathToFileURL } from "node:url"

export const FILTER_SKIP_REASON = "test name does not match pattern"

// A skipped case is `ok N - name # SKIP` with an optional reason. A genuine skip
// (`{ skip: true }`, `t.skip()`, a platform guard) has an empty or unrelated reason.
const SKIP_DIRECTIVE = /^[ \t]*(?:not ok|ok) \d+ - .*?# SKIP(?: (.*?))?[ \t]*$/gm

function summary(tap, label) {
  const match = tap.match(new RegExp(`^# ${label} (\\d+)$`, "m"))
  if (!match) throw new Error(`TAP output has no '# ${label}' summary line`)
  return Number(match[1])
}

/**
 * Enforce the Windows acceptance contract on one `node --test` TAP run.
 *
 * @param {{exitCode: number, tap: string, expectedPass: number}} run
 * @throws when the run did not pass cleanly, selected fewer cases, failed,
 *   cancelled, or skipped a case for anything other than the Node 20 filter.
 */
export function assertWindowsTap({ exitCode, tap, expectedPass }) {
  if (exitCode !== 0) {
    throw new Error(`node --test exited ${exitCode}; the selected Windows cases must pass`)
  }
  const pass = summary(tap, "pass")
  const fail = summary(tap, "fail")
  const cancelled = summary(tap, "cancelled")
  if (pass !== expectedPass) {
    throw new Error(`expected ${expectedPass} selected passes, got ${pass} (zero-run fallback)`)
  }
  if (fail !== 0) throw new Error(`expected 0 failures, got ${fail}`)
  if (cancelled !== 0) throw new Error(`expected 0 cancelled, got ${cancelled}`)
  for (const [, reason] of tap.matchAll(SKIP_DIRECTIVE)) {
    if (reason !== FILTER_SKIP_REASON) {
      throw new Error(`a selected Windows case skipped for an unexpected reason: ${reason ?? "<empty>"}`)
    }
  }
}

function main(argv) {
  const [tapPath, exitCode, expectedPass] = argv
  if (!tapPath || exitCode === undefined || expectedPass === undefined) {
    console.error("usage: node windows-ci-tap-guard.mjs <tap-file> <exit-code> <expected-pass>")
    process.exitCode = 2
    return
  }
  try {
    assertWindowsTap({ exitCode: Number(exitCode), tap: readFileSync(tapPath, "utf8"), expectedPass: Number(expectedPass) })
    console.log(`Windows TAP guard: ${expectedPass} selected cases passed, no genuine skips`)
  } catch (error) {
    console.error(`Windows TAP guard failed: ${error.message}`)
    process.exitCode = 1
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2))
}
