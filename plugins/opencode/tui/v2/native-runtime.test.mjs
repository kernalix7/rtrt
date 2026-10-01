import assert from "node:assert/strict"
import { EventEmitter } from "node:events"
import { performance } from "node:perf_hooks"
import test from "node:test"
import { createNativeController, NATIVE_REFRESH_EVENTS, registerNativeStatusline } from "./native-runtime.mjs"
import "./native-binary.cases.mjs"

function fixture() {
  const callbacks = new Map()
  const disposed = []
  const claims = []
  const updates = []
  const entry = { sessionID: () => "active", refresh: () => updates.push("refresh"), dispose: () => disposed.push("entry") }
  const ctx = {
    data: { on: (name, callback) => { callbacks.set(name, callback); return () => disposed.push(name) } },
    ui: { slot: (claim) => { claims.push(claim); return () => disposed.push("slot") } },
  }
  return { ctx, entry, callbacks, disposed, claims, updates }
}

test("registers an additive native prompt footer claim and forwards reactive input", () => {
  // Given
  const state = fixture()
  const render = (input) => input.sessionID
  // When
  const cleanup = registerNativeStatusline(state.ctx, state.entry, render)
  // Then
  assert.equal(typeof cleanup, "function")
  assert.equal(state.claims.length, 1)
  assert.equal(state.claims[0].append, "prompt.footer.status")
  assert.equal(state.claims[0].render({ sessionID: "rendered", mode: "normal", showDetails: true }), "rendered")
  cleanup()
})

test("uses real 2.0.20 event names without stream-delta subscriptions", () => {
  // Given
  const state = fixture()
  // When
  const cleanup = registerNativeStatusline(state.ctx, state.entry, () => undefined)
  // Then
  for (const name of ["session.status", "session.idle", "session.step.ended", "session.usage.updated", "session.model.selected", "model.updated"]) {
    assert.ok(state.callbacks.has(name), name)
  }
  assert.equal([...state.callbacks.keys()].some((name) => name.endsWith(".delta")), false)
  assert.equal(state.callbacks.has("session.updated"), false)
  cleanup()
})

for (const [event, expected] of [
  [{ type: "session.status", data: { sessionID: "active", status: { type: "busy" } } }, 1],
  [{ type: "session.status", data: { sessionID: "background", status: { type: "busy" } } }, 0],
  [{ type: "model.updated", data: {} }, 1],
]) {
  test(`scopes ${event.type} refresh for ${event.data.sessionID ?? "catalog"}`, () => {
    // Given
    const state = fixture()
    const cleanup = registerNativeStatusline(state.ctx, state.entry, () => undefined)
    // When
    state.callbacks.get(event.type)(event)
    // Then
    assert.equal(state.updates.length, expected)
    cleanup()
  })
}

test("cleanup releases slot, callbacks and controller exactly once", () => {
  // Given
  const state = fixture()
  const cleanup = registerNativeStatusline(state.ctx, state.entry, () => undefined)
  // When
  cleanup()
  cleanup()
  state.callbacks.get("session.idle")({ type: "session.idle", data: { sessionID: "active" } })
  // Then
  assert.deepEqual(state.disposed.toSorted(), ["entry", "slot", ...NATIVE_REFRESH_EVENTS].toSorted())
  assert.deepEqual(state.updates, [])
})

test("registration failure releases already-created resources", () => {
  // Given
  const state = fixture()
  state.ctx.ui.slot = () => { throw new Error("unavailable slot") }
  // When / Then
  assert.throws(() => registerNativeStatusline(state.ctx, state.entry, () => undefined), /unavailable slot/u)
  assert.ok(state.disposed.includes("entry"))
  assert.equal(state.disposed.length, state.callbacks.size + 1)
})

for (const failing of ["entry", "slot", "session.idle"]) {
  test(`cleanup attempts every disposer when ${failing} throws`, () => {
    // Given
    const state = fixture()
    const failure = new Error("disposal failed")
    const dispose = (name) => () => {
      state.disposed.push(name)
      if (name === failing) throw failure
    }
    state.entry.dispose = dispose("entry")
    state.ctx.ui.slot = () => dispose("slot")
    state.ctx.data.on = (name, callback) => {
      state.callbacks.set(name, callback)
      return dispose(name)
    }
    const cleanup = registerNativeStatusline(state.ctx, state.entry, () => undefined)
    // When
    let caught
    try { cleanup() } catch (error) { caught = error }
    cleanup()
    state.callbacks.get("session.idle")({ type: "session.idle", data: { sessionID: "active" } })
    // Then
    assert.deepEqual(state.disposed.toSorted(), ["entry", "slot", ...NATIVE_REFRESH_EVENTS].toSorted())
    assert.deepEqual(state.updates, [])
    assert.ok(caught instanceof AggregateError)
    assert.deepEqual(caught.errors, [failure])
  })
}

test("registration failure preserves its cause when cleanup also throws", () => {
  // Given
  const state = fixture()
  const registrationError = new Error("unavailable slot")
  const disposalError = new Error("disposal failed")
  state.ctx.ui.slot = () => { throw registrationError }
  state.entry.dispose = () => { state.disposed.push("entry"); throw disposalError }
  // When
  let caught
  try { registerNativeStatusline(state.ctx, state.entry, () => undefined) } catch (error) { caught = error }
  // Then
  assert.deepEqual(state.disposed.toSorted(), ["entry", ...NATIVE_REFRESH_EVENTS].toSorted())
  assert.ok(caught instanceof AggregateError)
  assert.equal(caught.errors[0], registrationError)
  assert.ok(caught.errors[1] instanceof AggregateError)
  assert.deepEqual(caught.errors[1].errors, [disposalError])
})

test("native controller honors explicit absolute binary and argument-only model", async () => {
  // Given
  const calls = []
  const spawn = (binary, args, options) => {
    calls.push({ binary, args, options })
    const child = new EventEmitter()
    child.stdout = new EventEmitter()
    child.kill = () => true
    queueMicrotask(() => {
      child.stdout.emit("data", Buffer.from(JSON.stringify({ v: 1, segments: [{ id: "style", text: "opt:full", tone: "accent", pri: 90 }] })))
      child.emit("close", 0)
    })
    return child
  }
  const controller = createNativeController({ bin: "/usr/local/bin/rtrt" }, () => {}, spawn)
  controller.setContext({ cwd: "/repo/space dir", session: "active", width: 80, model: "provider/chosen;literal" })
  // When
  await controller.refreshNow()
  // Then
  assert.equal(calls.length, 1)
  assert.equal(calls[0].binary, "/usr/local/bin/rtrt")
  assert.equal(calls[0].options.shell, false)
  assert.equal(calls[0].options.cwd, "/repo/space dir")
  assert.deepEqual(calls[0].args, ["statusline", "--opencode", "--cwd", "/repo/space dir", "--session", "active", "--width", "80", "--model", "provider/chosen;literal"])
  assert.equal(controller.getState().unavailable, false)
  controller.dispose()
})

test("native controller aborts its bounded subprocess on disposal", async () => {
  // Given
  const kills = []
  const child = new EventEmitter()
  child.stdout = new EventEmitter()
  child.kill = (signal) => { kills.push(signal); return true }
  const controller = createNativeController({ bin: "/usr/local/bin/rtrt" }, () => {}, () => child)
  const pending = controller.refreshNow()
  // When
  controller.dispose()
  await pending
  // Then
  assert.deepEqual(kills, ["SIGKILL"])
  assert.equal(controller.getState().unavailable, true)
})

test("native controller rejects oversized CLI output without leaking a running child", async () => {
  // Given
  const kills = []
  const spawn = () => {
    const child = new EventEmitter()
    child.stdout = new EventEmitter()
    child.kill = (signal) => { kills.push(signal); return true }
    queueMicrotask(() => child.stdout.emit("data", Buffer.alloc(65 * 1024, "x")))
    return child
  }
  const controller = createNativeController({ bin: "/usr/local/bin/rtrt" }, () => {}, spawn)
  // When
  await controller.refreshNow()
  // Then
  assert.ok(kills.length > 0)
  assert.ok(kills.every((signal) => signal === "SIGKILL"))
  assert.equal(controller.getState().unavailable, true)
  controller.dispose()
})

test("native controller shares one 1500ms deadline across binary candidates", async (t) => {
  // Given
  t.mock.timers.enable({ apis: ["setTimeout"] })
  let now = 0
  t.mock.method(performance, "now", () => now)
  const calls = []
  const kills = []
  const spawn = (binary) => {
    calls.push(binary)
    const child = new EventEmitter()
    child.stdout = new EventEmitter()
    child.kill = (signal) => { kills.push(signal); return true }
    return child
  }
  const controller = createNativeController({ bin: "/usr/local/bin/rtrt" }, () => {}, spawn)
  const pending = controller.refreshNow()
  // When
  now = 1500
  t.mock.timers.tick(1500)
  await pending
  // Then
  assert.deepEqual(calls, ["/usr/local/bin/rtrt"])
  assert.deepEqual(kills, ["SIGKILL"])
  assert.equal(controller.getState().unavailable, true)
  controller.dispose()
})
