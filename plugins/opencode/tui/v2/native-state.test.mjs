import assert from "node:assert/strict"
import test from "node:test"
import { activeSessionID, nativeSnapshot } from "./native-state.mjs"
import { cellWidth, initialStatuslineState, renderStatuslineViewText } from "../rtrt-statusline-core.mjs"

const tokens = { input: 100, output: 20, reasoning: 30, cache: { read: 200, write: 50 } }
const model = { id: "chosen", providerID: "provider" }
const assistant = {
  id: "answer", type: "assistant", agent: "build", content: [],
  model: { id: "answered", providerID: "provider" },
  time: { created: 1, completed: 2 }, tokens, cost: 999,
}
const session = {
  id: "active", projectID: "project", model, cost: 1.234, tokens,
  time: { created: 1, updated: 2 }, location: { directory: "/repo/active" },
}
const catalog = [
  { id: "chosen", modelID: "upstream-chosen", providerID: "provider", name: "Chosen Model", limit: { context: 2000 } },
  { id: "answered", modelID: "upstream-answered", providerID: "provider", name: "Answered Model", limit: { context: 800 } },
]

function fixture(overrides = {}) {
  const values = { session, messages: [assistant], catalog, selected: { providerID: "provider", modelID: "chosen" }, ...overrides }
  const reads = []
  return {
    reads,
    ctx: {
      location: { directory: "/repo/fallback" },
      ui: { router: { current: () => ({ type: "session", sessionID: "active" }) }, model: { current: () => values.selected } },
      data: {
        session: {
          get: (id) => { reads.push(id); return values.session },
          message: { list: () => values.messages },
          status: () => "running",
          cost: () => 9999,
        },
        location: {
          default: () => ({ directory: "/repo/default" }),
          model: { list: (location) => { reads.push(location.directory); return values.catalog } },
        },
      },
    },
  }
}

test("selects the native slot session before router state", () => {
  // Given
  const { ctx } = fixture()
  // When
  const result = activeSessionID(ctx, "slot-session")
  // Then
  assert.equal(result, "slot-session")
})

for (const route of [{ type: "home" }, { type: "plugin", id: "other", name: "other" }]) {
  test(`does not select a stale session on ${route.type}`, () => {
    // Given
    const { ctx } = fixture()
    ctx.ui.router.current = () => route
    // When
    const result = activeSessionID(ctx)
    // Then
    assert.equal(result, "")
  })
}

test("uses native cost and the completed response model context limit", () => {
  // Given
  const { ctx, reads } = fixture()
  // When
  const snapshot = nativeSnapshot(ctx, "active")
  // Then
  assert.equal(snapshot.cwd, "/repo/active")
  assert.deepEqual(reads, ["active", "/repo/active"])
  assert.equal(snapshot.economics.cost.text, "~$1.23")
  assert.equal(snapshot.economics.model.text, "Chosen Model")
  assert.equal(snapshot.economics.modelRef, "provider/chosen")
  assert.equal(snapshot.economics.status.text, "BUSY")
  assert.equal(snapshot.economics.context.used, 400)
  assert.equal(snapshot.economics.context.limit, 800)
  assert.equal(snapshot.economics.context.percent, 50)
  assert.equal(snapshot.economics.week.text, "N/A/not exposed")
})

test("uses prompt selection before the persisted session model", () => {
  // Given
  const { ctx } = fixture({ selected: { providerID: "provider", modelID: "answered" } })
  // When
  const snapshot = nativeSnapshot(ctx, "active")
  // Then
  assert.equal(snapshot.economics.model.text, "Answered Model")
  assert.equal(snapshot.economics.modelRef, "provider/answered")
})

test("falls back to the native session model when prompt selection is absent", () => {
  // Given
  const { ctx } = fixture({ selected: undefined })
  // When
  const snapshot = nativeSnapshot(ctx, "active")
  // Then
  assert.equal(snapshot.economics.modelRef, "provider/chosen")
})

test("home shows selected model without borrowing session economics", () => {
  // Given
  const { ctx, reads } = fixture()
  // When
  const snapshot = nativeSnapshot(ctx, "")
  // Then
  assert.equal(snapshot.cwd, "/repo/fallback")
  assert.deepEqual(reads, ["/repo/fallback"])
  assert.equal(snapshot.economics.model.text, "Chosen Model")
  assert.equal(snapshot.economics.cost.text, "N/A")
  assert.equal(snapshot.economics.context.text, "N/A")
  assert.equal(snapshot.economics.status.text, "UNKNOWN")
})

test("uses default native location when the launch location is absent", () => {
  // Given
  const { ctx } = fixture({ session: undefined })
  ctx.location = undefined
  // When
  const snapshot = nativeSnapshot(ctx, "missing")
  // Then
  assert.equal(snapshot.cwd, "/repo/default")
  assert.equal(snapshot.economics.status.text, "UNKNOWN")
})

test("zero native cost remains uncertain rather than invented savings", () => {
  // Given
  const { ctx } = fixture({ session: { ...session, cost: 0 } })
  // When
  const snapshot = nativeSnapshot(ctx, "active")
  // Then
  assert.equal(snapshot.economics.cost.text, "$0?")
})

test("incomplete and failed responses do not replace the last completed usage", () => {
  // Given
  const { ctx } = fixture({ messages: [
    assistant,
    { ...assistant, id: "incomplete", time: { created: 3 }, tokens: { ...tokens, input: 9000 } },
    { ...assistant, id: "failed", error: { name: "failure", message: "failed" } },
  ] })
  // When
  const snapshot = nativeSnapshot(ctx, "active")
  // Then
  assert.equal(snapshot.economics.context.percent, 50)
})

test("does not guess context capacity from a different catalog model", () => {
  // Given
  const { ctx } = fixture({ catalog: catalog.slice(0, 1) })
  // When
  const snapshot = nativeSnapshot(ctx, "active")
  // Then
  assert.equal(snapshot.economics.context.text, "N/A")
  assert.equal(snapshot.economics.context.used, 400)
})

for (const width of [1, 24, 40, 80, 120]) {
  test(`keeps the existing renderer bounded at ${width} columns`, () => {
    // Given
    const { ctx } = fixture()
    const { economics } = nativeSnapshot(ctx, "active")
    // When
    const output = renderStatuslineViewText(initialStatuslineState(), { width, economics })
    // Then
    assert.ok(output.length > 0)
    assert.ok(output.split("\n").every((line) => cellWidth(line) <= width))
  })
}
