import assert from "node:assert/strict"
import { chmod, mkdir, symlink } from "node:fs/promises"
import path from "node:path"
import test from "node:test"
import { resolveDashboardBinary } from "./runtime/dashboard-binary.js"
import { fixture, packageFixture, PACKAGE, VERSION } from "./test-fixtures/dashboard.mjs"

test("resolver selects exact-version platform npm package", async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  const expected = await packageFixture(root)
  // When
  const binary = await resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] })
  // Then
  assert.equal(binary, expected)
})

test("resolver accepts only version-scoped private cache package", async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.state, "packages", VERSION, "node_modules")
  const expected = await packageFixture(root)
  // When
  const binary = await resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [] })
  // Then
  assert.equal(binary, expected)
})

test("resolver rejects mismatched version without PATH or target fallback", async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  await packageFixture(root, "99.0.0")
  // When / Then
  await assert.rejects(resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] }))
})

test("resolver rejects symlinked package directory", async (t) => {
  // Given
  const f = await fixture(t)
  const realRoot = path.join(f.home, "real")
  await packageFixture(realRoot)
  const root = path.join(f.home, "node_modules")
  await mkdir(root)
  await symlink(path.join(realRoot, PACKAGE), path.join(root, PACKAGE))
  // When / Then
  await assert.rejects(resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] }))
})

test("resolver rejects group-writable executable", { skip: process.platform === "win32" }, async (t) => {
  // Given
  const f = await fixture(t)
  const root = path.join(f.home, "node_modules")
  const binary = await packageFixture(root)
  await chmod(binary, 0o775)
  // When / Then
  await assert.rejects(resolveDashboardBinary({ home: f.home, platform: "linux", arch: "x64", version: VERSION, roots: [root] }))
})
