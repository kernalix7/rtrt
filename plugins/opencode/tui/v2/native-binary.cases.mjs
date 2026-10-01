import assert from "node:assert/strict"
import { EventEmitter } from "node:events"
import test from "node:test"
import { createNativeController } from "./native-runtime.mjs"

function failingSpawn(calls) {
  return (binary) => {
    calls.push(binary)
    const child = new EventEmitter()
    child.stdout = new EventEmitter()
    child.kill = () => true
    queueMicrotask(() => child.emit("close", 1))
    return child
  }
}

function environmentBinary(t) {
  const previous = process.env.RTRT_BIN
  process.env.RTRT_BIN = "/environment/rtrt"
  t.after(() => {
    if (previous === undefined) delete process.env.RTRT_BIN
    else process.env.RTRT_BIN = previous
  })
}

test("explicit binary failure never falls back to environment or PATH", async (t) => {
  // Given
  environmentBinary(t)
  const calls = []
  const controller = createNativeController({ bin: "/explicit/rtrt" }, () => {}, failingSpawn(calls))
  t.after(() => controller.dispose())
  // When
  await controller.refreshNow()
  // Then
  assert.deepEqual(calls, ["/explicit/rtrt"])
  assert.equal(controller.getState().unavailable, true)
})

test("explicit binary spawn errors never select another executable", async (t) => {
  // Given
  const calls = []
  const controller = createNativeController({ bin: "/missing/rtrt" }, () => {}, (binary) => {
    calls.push(binary)
    throw new Error("missing binary")
  })
  t.after(() => controller.dispose())
  // When
  await controller.refreshNow()
  // Then
  assert.deepEqual(calls, ["/missing/rtrt"])
  assert.equal(controller.getState().unavailable, true)
})

for (const bin of ["", "rtrt", "./rtrt", null, undefined, 42]) {
  test(`invalid explicit bin ${JSON.stringify(bin)} stays unavailable without spawning`, async (t) => {
    // Given
    const calls = []
    const controller = createNativeController({ bin }, () => {}, failingSpawn(calls))
    t.after(() => controller.dispose())
    // When
    await controller.refreshNow()
    // Then
    assert.deepEqual(calls, [])
    assert.equal(controller.getState().unavailable, true)
  })
}

test("omitted optional binary uses default lookup and stays unavailable when absent", async (t) => {
  // Given
  environmentBinary(t)
  const calls = []
  const controller = createNativeController({}, () => {}, failingSpawn(calls))
  t.after(() => controller.dispose())
  // When
  await controller.refreshNow()
  // Then
  assert.deepEqual(calls, ["/environment/rtrt", "rtrt"])
  assert.equal(controller.getState().unavailable, true)
})

test("disposed native controller never spawns or publishes again", async () => {
  // Given
  const calls = []
  const updates = []
  const controller = createNativeController({}, (state) => updates.push(state), failingSpawn(calls))
  controller.dispose()
  // When
  controller.setContext({ cwd: "/repo/changed", session: "later" })
  controller.start()
  controller.request(true)
  await controller.refreshNow()
  // Then
  assert.deepEqual(calls, [])
  assert.deepEqual(updates, [])
})
