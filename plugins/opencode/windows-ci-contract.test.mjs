import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { readFileSync } from "node:fs"
import { test } from "node:test"
import { fileURLToPath } from "node:url"
import { assertWindowsTap } from "./windows-ci-tap-guard.mjs"

const root = fileURLToPath(new URL("../../", import.meta.url))
const ci = JSON.parse(execFileSync("ruby", ["-rpsych", "-rjson", "-e",
  "puts JSON.generate(Psych.safe_load_file(ARGV.fetch(0), permitted_classes: [], aliases: false))",
  `${root}.github/workflows/ci.yml`], { encoding: "utf8" }))

const JOB = "windows-dashboard-acl"
const DASHBOARD_FILTER = "tests::machine_startup_accepts_plain_home_override_only_with_private_token"
const CORE_FILTER = "windows_acl::tests::private_state_accepts_owner_only_and_rejects_inherited_acl"
const NODE_FILES = [
  "plugins/opencode/dashboard-acl.test.mjs",
  "plugins/opencode/dashboard-binary.test.mjs",
  "plugins/opencode/dashboard-open.test.mjs",
]
const TAP_GUARD = "windows-ci-tap-guard.mjs"
const ACCEPTANCE_PASS = 24
const SYSTEM_DRIVE_PREFIX = "rtrt-win-ci-"
const SYSTEM_DRIVE_BINARY_PASS = 18
const REQUIRED_JOBS = [
  "fmt", "release-contract", "clippy", "feature-lanes", "msrv",
  "test", "audit", "license-inventory", "opencode", JOB,
]

function runOf(step) {
  return step.run ?? ""
}

function findRunStep(job, fragment) {
  return job.steps.find((step) => runOf(step).includes(fragment))
}

function assertWindowsDashboardContract(job) {
  assert.ok(job, "windows-dashboard-acl job is required")
  assert.equal(job["runs-on"], "windows-latest", "job must run on windows-latest")
  assert.equal(Number(job["timeout-minutes"]), 45, "timeout must stay at the reviewed 45 minutes")
  assert.equal(job.if, undefined, "job must not be conditionally skipped")

  for (const step of job.steps) {
    assert.notEqual(step["continue-on-error"], true, "no step may swallow a failure")
  }

  const checkout = job.steps.find((step) => step.uses?.startsWith("actions/checkout@"))
  assert.ok(checkout, "checkout step is required")
  assert.equal(checkout.with?.["persist-credentials"], false, "checkout must not persist credentials")

  const rust = job.steps.find((step) => step.uses?.startsWith("dtolnay/rust-toolchain@"))
  assert.ok(rust, "pinned Rust toolchain step is required")
  assert.equal(rust.with?.toolchain, "stable", "Rust toolchain must be stable")
  assert.match(rust.uses, /@[0-9a-f]{40}$/, "Rust toolchain action must be commit-pinned")

  const cache = job.steps.find((step) => step.uses?.startsWith("Swatinem/rust-cache@"))
  assert.ok(cache, "Cargo cache step is required")
  assert.match(cache.uses, /@[0-9a-f]{40}$/, "Cargo cache action must be commit-pinned")

  const node = job.steps.find((step) => step.uses?.startsWith("actions/setup-node@"))
  assert.ok(node, "setup-node step is required")
  assert.equal(node.with?.["node-version"], 20, "Node engine contract is >=20")

  const build = findRunStep(job, "cargo build --locked -p rtrt-dashboard")
  assert.ok(build, "must build the production dashboard executable with --locked")

  const pin = findRunStep(job, "Resolve-Path")
  assert.ok(pin, "must resolve the actual built executable path")
  assert.match(runOf(pin), /RTRT_TEST_DASHBOARD_EXE/, "must pass the path via RTRT_TEST_DASHBOARD_EXE")
  assert.match(runOf(pin), /-ErrorAction Stop/, "a missing executable must terminate the step")
  assert.match(runOf(pin), /IsPathRooted/, "the executable path must be asserted absolute")
  assert.match(runOf(pin), /GITHUB_ENV/, "the path must persist to the acceptance steps")

  const guard = job.steps.find((step) => runOf(step).includes("Test-Path") && runOf(step).includes("RTRT_TEST_DASHBOARD_EXE"))
  assert.ok(guard, "an explicit missing-executable guard is required")
  assert.match(runOf(guard), /throw/, "the guard must fail rather than skip")

  const dashboard = findRunStep(job, DASHBOARD_FILTER)
  assert.ok(dashboard, `dashboard startup filter is required: ${DASHBOARD_FILTER}`)
  assert.match(runOf(dashboard), /--exact/, "the filter must select the exact full test path")
  assert.match(runOf(dashboard), /test result: ok\. 1 passed/, "a zero-selected green run must fail")

  const core = findRunStep(job, CORE_FILTER)
  assert.ok(core, `core ACL filter is required: ${CORE_FILTER}`)
  assert.match(runOf(core), /--exact/, "the filter must select the exact full test path")
  assert.match(runOf(core), /test result: ok\. 1 passed/, "a zero-selected green run must fail")

  const systemDrive = job.steps.find((step) => runOf(step).includes("SystemDrive"))
  assert.ok(systemDrive, "the system-drive resolver step is required")
  assert.equal(job.steps.filter((step) => runOf(step).includes("--test-name-pattern=Windows")).length, 1,
    "both selected suites must run in the same copied-project step")
  const sd = runOf(systemDrive)
  assert.match(sd, /\$driveRoot -cne 'C:\\'/, "copied-project fixture must use the C system drive")
  const accepted = sd
  assert.match(accepted, /--test-name-pattern=Windows/, "only Windows cases may be selected")
  for (const file of NODE_FILES) {
    assert.ok(accepted.includes(file.split("/").at(-1)), `node acceptance misses ${file}`)
  }
  assert.match(accepted, /--test-reporter=tap/, "acceptance must emit TAP for the guard")
  assert.match(accepted, /Tee-Object -FilePath \$acceptanceTap/, "acceptance TAP must be captured separately")
  assert.match(accepted, /\$acceptanceCode = \$LASTEXITCODE/, "the acceptance native exit code must be captured immediately")
  assert.match(accepted, new RegExp(`\\$guard \\$acceptanceTap \\$acceptanceCode ${ACCEPTANCE_PASS}`), "the acceptance TAP guard must require every selected pass")
  assert.match(accepted, /if \(\$LASTEXITCODE -ne 0\)/, "a failed guard must terminate the step")
  assert.doesNotMatch(accepted, /# skipped 0/, "the false Node20 '# skipped 0' total must stay removed")

  assert.match(sd, /\$env:SystemDrive/, "must resolve the actual system drive")
  assert.match(sd, new RegExp(`'${SYSTEM_DRIVE_PREFIX}' \\+ \\[guid\\]::NewGuid\\(\\)\\.ToString\\('N'\\)`), "fixture must use the fixed prefix plus a GUID")
  assert.match(sd, /Copy-Item -Path \(Join-Path \$env:GITHUB_WORKSPACE 'plugins\\opencode'\)/, "must copy plugins/opencode into the fixture")
  assert.match(sd, /Copy-Item -Path \(Join-Path \$env:GITHUB_WORKSPACE 'install\.ps1'\)/, "must copy install.ps1 into the fixture")
  assert.match(sd, /Push-Location \$copiedProject/, "the real tests must execute from the copied project")
  assert.ok(sd.indexOf("try {") >= 0 && sd.indexOf("try {") < sd.indexOf("New-Item -ItemType Directory"), "creation must be inside try/finally")
  assert.ok(sd.indexOf("try {") < sd.indexOf("Copy-Item -Path"), "partial copies must be cleaned up")
  const acceptanceCall = "node --test --test-reporter=tap --test-name-pattern=Windows dashboard-acl.test.mjs dashboard-binary.test.mjs dashboard-open.test.mjs"
  const binaryCall = "node --test --test-reporter=tap --test-name-pattern=Windows dashboard-binary.test.mjs"
  assert.ok(sd.indexOf("Push-Location $copiedProject") < sd.indexOf(acceptanceCall), "acceptance must execute from the copied module")
  assert.ok(sd.indexOf(acceptanceCall) < sd.indexOf(`$guard $acceptanceTap $acceptanceCode ${ACCEPTANCE_PASS}`), "acceptance must be guarded")
  assert.ok(sd.indexOf(`$guard $acceptanceTap $acceptanceCode ${ACCEPTANCE_PASS}`) < sd.indexOf(binaryCall), "24 selected passes must precede the 18 binary passes")
  assert.match(sd, /\$acceptanceTap = Join-Path \$env:RUNNER_TEMP 'windows-acceptance\.tap'/)
  assert.match(sd, /\$binaryTap = Join-Path \$env:RUNNER_TEMP 'windows-system-drive-binary\.tap'/)
  assert.match(sd, /node --test --test-reporter=tap --test-name-pattern=Windows dashboard-binary\.test\.mjs/, "the real binary/resolver suite must run from the copy")
  assert.match(sd, /\$binaryCode = \$LASTEXITCODE/, "the binary native exit code must be captured immediately")
  assert.equal((sd.match(/node --test --test-reporter=tap --test-name-pattern=Windows /g) ?? []).length, 2,
    "only the original 24-case and 18-case calls may run")
  assert.match(sd, new RegExp(`\\$guard \\$binaryTap \\$binaryCode ${SYSTEM_DRIVE_BINARY_PASS}`), "the copied-project TAP guard must require every selected binary pass")
  assert.match(sd, /Get-Acl -LiteralPath \$driveRoot/, "the system-drive root ACL must be read")
  assert.match(sd, /SYSTEM_DRIVE_ROOT_ACE=.*\$\(\$ace\.InheritanceFlags\).*\$\(\$ace\.PropagationFlags\)/, "root ACE metadata must distinguish inherited-only from effective permissions")
  assert.doesNotMatch(sd, /Set-Acl/, "the system-drive step must not mutate any ACL")
  assert.doesNotMatch(sd, /SYSTEM_DRIVE_ROOT_OWNER[^\n]*(?:-eq|-ne|-ceq|-cne|Assert)/, "the root owner must be recorded, not required to be uniform")
  assert.match(sd, /\$preservedExe = \$env:RTRT_TEST_DASHBOARD_EXE/, "the original absolute exe path must be preserved")
  assert.equal((sd.match(/-cne \$preservedExe/g) ?? []).length, 2, "the executable path must be checked after each invocation")
  assert.match(sd, /Pop-Location/, "the working directory must be restored")
  assert.match(sd, /Remove-Item -LiteralPath \$fixtureProject -Recurse -Force/, "only the generated fixture project may be removed")
  assert.match(sd, new RegExp(`\\$fixtureProject -like \\(\\$driveRoot \\+ '${SYSTEM_DRIVE_PREFIX}\\*'\\)`), "cleanup must be scoped to the generated prefix")

  const refusal = job.steps.find((step) => step.name === "Prove D-root binary owner refusal")
  assert.ok(refusal, "the actual D-root refusal is mandatory and separate from both TAP counts")
  assert.equal(refusal.shell, "pwsh")
  assert.equal(refusal.if, undefined)
  assert.ok(job.steps.indexOf(refusal) > job.steps.indexOf(systemDrive), "D-root proof must follow copied-project acceptance")
  const proof = runOf(refusal)
  assert.match(proof, /path\.win32\.parse\(checkout\)\.root\.toUpperCase\(\) !== "D:\\\\"/, "must require the actual checkout drive to be D")
  assert.match(proof, /path\.join\(checkout, "plugins", "opencode", "runtime", "dashboard-acl\.ps1"\)/, "must execute unchanged production policy")
  assert.match(proof, /RTRT_ACL_PATH: driveRoot, RTRT_ACL_ACTION: "binary-check"/, "must test the actual D root")
  assert.match(proof, /env: \{ SystemRoot: process\.env\.SystemRoot, RTRT_ACL_PATH: driveRoot, RTRT_ACL_ACTION: "binary-check" \}/, "child must have exactly the three required environment keys")
  assert.match(proof, /-NoProfile.*-NonInteractive.*-Command/, "PS5 child must not run a profile")
  assert.match(proof, /timeout: 45000, maxBuffer: 4096/, "child must remain bounded")
  assert.match(proof, /-ceq 'Untrusted binary owner'/, "only the exact policy exception proves refusal")
  assert.match(proof, /\| Out-Null; \[Console\]::Out\.WriteLine\('ACCEPTED'\)/, "policy's item output must not masquerade as the refusal marker")
  assert.match(proof, /D_ROOT_REFUSED_UNTRUSTED_BINARY_OWNER/, "refusal must have an explicit safe marker")
  assert.match(proof, /stdout\.trim\(\) !== "D_ROOT_REFUSED_UNTRUSTED_BINARY_OWNER" \|\| stderr\.trim\(\) !== ""/, "unexpected acceptance, errors, or output must fail")
  assert.match(proof, /if \(\$LASTEXITCODE -ne 0\) \{ throw/, "failed child/proof must fail the job")
  assert.doesNotMatch(proof, /Set-Acl|New-Item|Copy-Item|Remove-Item|Tee-Object|windows-ci-tap-guard/, "D proof must be read-only and outside TAP counts")
}

function dropStepsWhere(job, predicate) {
  job.steps = job.steps.filter((step) => !predicate(step))
}

function replaceInStep(job, fragment, replacement) {
  const step = findRunStep(job, fragment)
  assert.ok(step, `fixture has no step containing ${fragment}`)
  step.run = step.run.replaceAll(fragment, replacement)
}

test("windows-dashboard-acl executes the real Windows acceptance contract", () => {
  // Given: the workflow Actions actually runs.
  const job = ci.jobs[JOB]
  // When: its build, env, Rust-regression, Node-acceptance, and system-drive steps are inspected.
  // Then: the shipped job satisfies the reviewed Windows contract.
  assert.doesNotThrow(() => assertWindowsDashboardContract(job))
})

test("Windows PS5 installer parser gate runs read-only before the dashboard build", () => {
  // Given: the job's checkout, Node setup, and costly Windows acceptance sequence.
  const job = ci.jobs[JOB]
  const nodeIndex = job.steps.findIndex((step) => step.uses?.startsWith("actions/setup-node@"))
  const parseIndex = job.steps.findIndex((step) => step.name === "Parse installer with Windows PowerShell 5.1")
  const buildIndex = job.steps.findIndex((step) => runOf(step).includes("cargo build --locked -p rtrt-dashboard"))
  // When: the actual parser invocation and ordering are inspected.
  // Then: it parses the original file in a bounded PS5 child without executing it.
  assert.ok(nodeIndex >= 0 && parseIndex > nodeIndex && buildIndex > parseIndex)
  const step = job.steps[parseIndex]
  assert.equal(step.shell, "pwsh")
  assert.equal(step.if, undefined)
  const source = runOf(step)
  assert.match(source, /path\.win32\.join\(process\.env\.SystemRoot, "System32", "WindowsPowerShell", "v1\.0", "powershell\.exe"\)/)
  assert.match(source, /-NoProfile.*-NonInteractive.*-Command/)
  assert.match(source, /path\.resolve\("install\.ps1"\)/)
  assert.match(source, /env: \{ SystemRoot: process\.env\.SystemRoot, RTRT_INSTALL_PATH: installer \}/)
  assert.match(source, /Parser\]::ParseFile\(\$env:RTRT_INSTALL_PATH,\[ref\]\$parseTokens,\[ref\]\$parseErrors\)/)
  assert.match(source, /\$parseErrors\.Count/)
  assert.match(source, /ErrorId.*StartLineNumber.*StartColumnNumber.*Message/)
  assert.match(source, /timeout: 45000, maxBuffer: 4096/)
  assert.doesNotMatch(source, /ParseInput|Invoke-Expression|Set-Acl|\.NET\.Encoding|ReadAllText/)
})

test("Windows PS5 installer source stays pure ASCII without a BOM", () => {
  // Given: the shipped install.ps1 that BOMless Windows PowerShell 5.1 reads as ANSI.
  const bytes = readFileSync(`${root}install.ps1`)
  // When: the raw bytes are inspected before any decoding.
  const nonAscii = [...bytes].filter((byte) => byte > 0x7f)
  // Then: there is no UTF-8 BOM and every byte is ASCII, so the PS5 ANSI read
  // cannot fold a multi-byte character into a smart-quote token delimiter.
  assert.notDeepEqual(
    [...bytes.subarray(0, 3)],
    [0xef, 0xbb, 0xbf],
    "install.ps1 must not begin with a UTF-8 BOM",
  )
  assert.deepEqual(
    nonAscii,
    [],
    "install.ps1 must stay ASCII-only so Windows PowerShell 5.1 tokenizes every byte identically",
  )
})

test("each weakened Windows dashboard ACL job structure is rejected", () => {
  // Given: the shipped job and one-contract-clause removals applied in turn.
  const baseline = ci.jobs[JOB]
  const mutations = [
    (job) => { job["runs-on"] = "ubuntu-latest" },
    (job) => { job["timeout-minutes"] = 5 },
    (job) => { job.if = "false" },
    (job) => { job.steps[0]["continue-on-error"] = true },
    (job) => dropStepsWhere(job, (step) => step.uses?.startsWith("actions/checkout@")),
    (job) => dropStepsWhere(job, (step) => step.uses?.startsWith("dtolnay/rust-toolchain@")),
    (job) => dropStepsWhere(job, (step) => step.uses?.startsWith("Swatinem/rust-cache@")),
    (job) => dropStepsWhere(job, (step) => step.uses?.startsWith("actions/setup-node@")),
    (job) => dropStepsWhere(job, (step) => runOf(step).includes("cargo build --locked -p rtrt-dashboard")),
    (job) => dropStepsWhere(job, (step) => runOf(step).includes("Resolve-Path")),
    (job) => dropStepsWhere(job, (step) => runOf(step).includes("Test-Path")),
    (job) => dropStepsWhere(job, (step) => runOf(step).includes(DASHBOARD_FILTER)),
    (job) => dropStepsWhere(job, (step) => runOf(step).includes(CORE_FILTER)),
    (job) => dropStepsWhere(job, (step) => runOf(step).includes("dashboard-open.test.mjs")),
    (job) => replaceInStep(job, "GITHUB_ENV", "CONSOLE"),
    (job) => replaceInStep(job, "-ErrorAction Stop", "-ErrorAction Continue"),
    (job) => replaceInStep(job, "[System.IO.Path]::IsPathRooted($exe)", "$true"),
    (job) => replaceInStep(job, DASHBOARD_FILTER, "machine_startup_accepts_plain_home_override_only_with_private_token"),
    (job) => replaceInStep(job, " --exact", ""),
    (job) => replaceInStep(job, "test result: ok. 1 passed", "test result"),
    (job) => replaceInStep(job, "--test-name-pattern=Windows", "--test-name-pattern=."),
    (job) => replaceInStep(job, "dashboard-open.test.mjs", ""),
    (job) => replaceInStep(job, `$guard $acceptanceTap $acceptanceCode ${ACCEPTANCE_PASS}`, "true"),
    (job) => {
      const step = findRunStep(job, `$guard $acceptanceTap $acceptanceCode ${ACCEPTANCE_PASS}`)
      step.run += "\n          if ($output -notmatch '(?m)^# skipped 0') { throw 'weakened' }"
    },
    (job) => dropStepsWhere(job, (step) => runOf(step).includes("SystemDrive")),
    (job) => replaceInStep(job, "node --test --test-reporter=tap --test-name-pattern=Windows dashboard-binary.test.mjs", "Write-Host 'resolver skipped'"),
    (job) => replaceInStep(job, "Remove-Item -LiteralPath $fixtureProject -Recurse -Force", "Write-Host 'no cleanup'"),
    (job) => replaceInStep(job, "Get-Acl -LiteralPath $driveRoot", "Write-Host 'no acl'"),
    (job) => replaceInStep(job, `'${SYSTEM_DRIVE_PREFIX}'`, "'fixture-'"),
    (job) => replaceInStep(job, `$guard $binaryTap $binaryCode ${SYSTEM_DRIVE_BINARY_PASS}`, `$guard $binaryTap $binaryCode ${ACCEPTANCE_PASS}`),
    (job) => replaceInStep(job, "$preservedExe", "$env:RTRT_TEST_DASHBOARD_EXE"),
    (job) => {
      const step = findRunStep(job, "Remove-Item -LiteralPath $fixtureProject -Recurse -Force")
      step.run += "\n          Set-Acl -LiteralPath $driveRoot -AclObject $rootAcl"
    },
  ]
  // When / Then: every weakened structure fails the contract, including a missing job.
  assert.throws(() => assertWindowsDashboardContract(undefined))
  for (const mutate of mutations) {
    const fixture = structuredClone(baseline)
    mutate(fixture)
    assert.throws(() => assertWindowsDashboardContract(fixture), "mutation escaped the contract")
  }
})

test("copied-project order, cleanup, and D-root refusal cannot be weakened", () => {
  // Given: one independently weakened safety boundary per cloned Windows job.
  const baseline = ci.jobs[JOB]
  const mutations = [
    (job) => {
      const step = findRunStep(job, "SystemDrive")
      step.run = step.run.replace("try {\n", "")
    },
    (job) => replaceInStep(job, "Push-Location $copiedProject", "Push-Location $env:GITHUB_WORKSPACE"),
    (job) => replaceInStep(job, "$guard $acceptanceTap $acceptanceCode 24", "Write-Host '24 skipped'"),
    (job) => replaceInStep(job, "dashboard-acl.test.mjs dashboard-binary.test.mjs dashboard-open.test.mjs", "dashboard-binary.test.mjs"),
    (job) => replaceInStep(job, "$acceptanceTap $acceptanceCode 24", "$binaryTap $binaryCode 18"),
    (job) => replaceInStep(job, "$binaryCode = $LASTEXITCODE", "$binaryCode = 0"),
    (job) => replaceInStep(job, "if ($env:RTRT_TEST_DASHBOARD_EXE -cne $preservedExe)", "if ($false)"),
    (job) => dropStepsWhere(job, (step) => step.name === "Prove D-root binary owner refusal"),
    (job) => replaceInStep(job, "D_ROOT_REFUSED_UNTRUSTED_BINARY_OWNER", "ACCEPT"),
    (job) => replaceInStep(job, "-ceq 'Untrusted binary owner'", "-like '*'"),
    (job) => {
      const step = job.steps.find((candidate) => candidate.name === "Prove D-root binary owner refusal")
      step.run = step.run.replace("timeout: 45000, maxBuffer: 4096", "timeout: 90000, maxBuffer: 4096")
    },
    (job) => replaceInStep(job, "RTRT_ACL_PATH: driveRoot", "RTRT_ACL_PATH: checkout"),
    (job) => {
      const step = job.steps.find((candidate) => candidate.name === "Prove D-root binary owner refusal")
      step.run = step.run.replace("-NoProfile", "-Profile")
    },
  ]
  // When / Then: each mutation must break the structural contract.
  for (const [index, mutate] of mutations.entries()) {
    const fixture = structuredClone(baseline)
    mutate(fixture)
    assert.throws(() => assertWindowsDashboardContract(fixture), `mutation ${index} escaped the copied-project/refusal contract`)
  }
})

function tapOf({ pass, fail = 0, cancelled = 0, skipped = 0, skipLines = [] }) {
  return [
    "TAP version 13",
    ...skipLines,
    `1..${pass + fail + cancelled}`,
    `# tests ${pass + fail + cancelled + skipped}`,
    `# pass ${pass}`,
    `# fail ${fail}`,
    `# cancelled ${cancelled}`,
    `# skipped ${skipped}`,
    "# todo 0",
  ].join("\n")
}

const FILTER_SKIP_LINE = "ok 1 - an unrelated non-Windows case # SKIP test name does not match pattern"

test("Windows TAP guard accepts Node20 filter-only skips and Node24 omissions", () => {
  // Given: Node20 emits a SKIP line for every non-matching name; Node24 omits them.
  const node20 = tapOf({ pass: ACCEPTANCE_PASS, skipped: 12, skipLines: [FILTER_SKIP_LINE] })
  const node24 = tapOf({ pass: ACCEPTANCE_PASS })
  // When / Then: both engines pass because no selected case skipped for a real reason.
  assert.doesNotThrow(() => assertWindowsTap({ exitCode: 0, tap: node20, expectedPass: ACCEPTANCE_PASS }))
  assert.doesNotThrow(() => assertWindowsTap({ exitCode: 0, tap: node24, expectedPass: ACCEPTANCE_PASS }))
})

test("Windows TAP guard rejects a genuine selected skip", () => {
  // Given: a selected case skipped with an empty reason and one with a platform reason.
  const bare = tapOf({ pass: ACCEPTANCE_PASS, skipped: 1, skipLines: ["ok 2 - Windows case # SKIP"] })
  const platform = tapOf({ pass: ACCEPTANCE_PASS, skipped: 1, skipLines: ["ok 3 - Windows case # SKIP platform unavailable"] })
  // When / Then: neither is the Node20 filter reason, so both fail.
  assert.throws(() => assertWindowsTap({ exitCode: 0, tap: bare, expectedPass: ACCEPTANCE_PASS }), /unexpected reason/)
  assert.throws(() => assertWindowsTap({ exitCode: 0, tap: platform, expectedPass: ACCEPTANCE_PASS }), /unexpected reason/)
})

test("Windows TAP guard rejects incomplete, failed, cancelled, and non-zero runs", () => {
  // Given: one malformed run per failure mode.
  // When / Then: each trips the matching guard clause.
  assert.throws(() => assertWindowsTap({ exitCode: 0, tap: tapOf({ pass: ACCEPTANCE_PASS - 1 }), expectedPass: ACCEPTANCE_PASS }), /zero-run fallback/)
  assert.throws(() => assertWindowsTap({ exitCode: 0, tap: tapOf({ pass: ACCEPTANCE_PASS, fail: 1 }), expectedPass: ACCEPTANCE_PASS }), /expected 0 failures/)
  assert.throws(() => assertWindowsTap({ exitCode: 0, tap: tapOf({ pass: ACCEPTANCE_PASS, cancelled: 1 }), expectedPass: ACCEPTANCE_PASS }), /expected 0 cancelled/)
  assert.throws(() => assertWindowsTap({ exitCode: 1, tap: tapOf({ pass: ACCEPTANCE_PASS }), expectedPass: ACCEPTANCE_PASS }), /exited 1/)
  assert.throws(() => assertWindowsTap({ exitCode: 0, tap: "TAP version 13", expectedPass: ACCEPTANCE_PASS }), /no '# pass' summary/)
})

test("license, MSRV, and peer CI gates remain present and unweakened", () => {
  // Given: the full parsed CI job set.
  // When: every preserved job and its gate shape is inspected.
  // Then: no other gate was dropped or made conditional by this change.
  for (const name of REQUIRED_JOBS) {
    assert.ok(ci.jobs[name], `CI job ${name} must be preserved`)
  }
  assert.equal(ci.jobs["license-inventory"]["runs-on"], "ubuntu-latest")
  assert.equal(ci.jobs["license-inventory"].if, undefined)
  assert.equal(ci.jobs.msrv["runs-on"], "ubuntu-latest")
  assert.equal(ci.jobs.msrv.if, undefined)
  assert.equal(ci.jobs.audit["runs-on"], "ubuntu-latest")
})
