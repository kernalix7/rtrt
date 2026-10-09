import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import test from "node:test"
import { runInNewContext } from "node:vm"

const vendor = new URL("../../crates/rtrt-dashboard/ui/vendor/", import.meta.url)
const modulus = 4294967296
const exportsExpected = [
  "FDLayout", "FDLayoutConstants", "FDLayoutEdge", "FDLayoutNode", "DimensionD",
  "HashMap", "HashSet", "IGeometry", "IMath", "Integer", "Point", "PointD",
  "RandomSeed", "RectangleD", "Transform", "UniqueIDGeneretor", "Quicksort",
  "LinkedList", "LGraphObject", "LGraph", "LEdge", "LGraphManager", "LNode",
  "Layout", "LayoutConstants", "NeedlemanWunsch", "Matrix", "SVD",
].sort()

function loadBundle(name, dependencies = {}) {
  const file = new URL(`${name}.js`, vendor)
  const module = { exports: {} }
  runInNewContext(readFileSync(file, "utf8"), {
    module,
    exports: module.exports,
    require(id) {
      assert.ok(Object.hasOwn(dependencies, id), `Unexpected dependency: ${id}`)
      return dependencies[id]
    },
  }, { filename: file.pathname, timeout: 1000 })
  return module.exports
}

// Exact integer vectors computed with BigInt arithmetic, not the bundled generator.
const vectors = [
  [0, [1013904223, 1196435762, 3519870697, 2868466484, 1649599747, 2670642822]],
  [1, [1015568748, 1586005467, 2165703038, 3027450565, 217083232, 1587069247]],
  [42, [1083814273, 378494188, 2479403867, 955863294, 1613448261, 110225632]],
  [2147483648, [3161387871, 3343919410, 1372387049, 720982836, 3797083395, 523159174]],
  [4294967295, [1012239698, 806866057, 579071060, 2709482403, 3082116262, 3754216397]],
]

test("full CommonJS bundle keeps exports and default constructor/static API", () => {
  // Given
  const library = loadBundle("layout-base")
  const { RandomSeed } = library
  // When
  const instance = new RandomSeed()
  // Then
  assert.equal(typeof library, "function")
  assert.deepEqual(Object.keys(library).sort(), exportsExpected)
  assert.ok(instance instanceof RandomSeed)
  assert.deepEqual(Object.keys(instance), [])
  assert.deepEqual(Object.keys(RandomSeed).sort(), ["nextDouble", "seed", "x"])
  assert.equal(RandomSeed.length, 0)
  assert.equal(RandomSeed.nextDouble.length, 0)
  assert.equal(RandomSeed.seed, 1)
  assert.equal(RandomSeed.x, 0)
})

test("constructing RandomSeed leaves externally assigned static state intact", () => {
  // Given
  const { RandomSeed } = loadBundle("layout-base")
  RandomSeed.seed = 42
  RandomSeed.x = 0.5
  // When
  new RandomSeed()
  // Then
  assert.equal(RandomSeed.seed, 42)
  assert.equal(RandomSeed.x, 0.5)
})

test("default seed produces the fixed first sample without a receiver or arguments", () => {
  // Given
  const { RandomSeed } = loadBundle("layout-base")
  const next = RandomSeed.nextDouble
  // When
  const sample = next()
  // Then
  assert.equal(sample, 1015568748 / modulus)
  assert.equal(RandomSeed.seed, 1015568748)
  assert.equal(RandomSeed.x, sample)
})

for (const [seed, states] of vectors) {
  test(`seed ${seed} produces its golden state/output trace`, () => {
    // Given
    const { RandomSeed } = loadBundle("layout-base")
    RandomSeed.seed = seed
    // When
    const trace = states.map(() => {
      const sample = RandomSeed.nextDouble()
      return [RandomSeed.seed, RandomSeed.x, sample]
    })
    // Then
    assert.deepEqual(trace, states.map(state => [state, state / modulus, state / modulus]))
  })
}

const boundaries = [
  ["negative zero", -0, 1013904223],
  ["positive fraction", 1.75, 1015568748],
  ["negative fraction", -1.75, 1012239698],
  ["positive subunit fraction", 0.75, 1013904223],
  ["negative subunit fraction", -0.75, 1013904223],
  ["negative integer", -1, 1012239698],
  ["uint32 wrap", 4294967296, 1013904223],
  ["uint32 wrap plus one", 4294967297, 1015568748],
  ["negative wrap", -4294967297, 1012239698],
  ["largest safe integer", Number.MAX_SAFE_INTEGER, 1012239698],
  ["smallest safe integer", Number.MIN_SAFE_INTEGER, 1015568748],
  ["largest finite number", Number.MAX_VALUE, 1013904223],
  ["NaN coerces to zero", NaN, 1013904223],
  ["positive infinity coerces to zero", Infinity, 1013904223],
  ["negative infinity coerces to zero", -Infinity, 1013904223],
  ["inclusive zero output", 634785765, 0],
  ["largest output below one", 653637408, 4294967295],
]
for (const [name, seed, state] of boundaries) {
  test(`seed boundary: ${name}`, () => {
    // Given
    const { RandomSeed } = loadBundle("layout-base")
    RandomSeed.seed = seed
    // When
    const sample = RandomSeed.nextDouble()
    // Then
    assert.equal(RandomSeed.seed, state)
    assert.equal(sample, state / modulus)
    assert.equal(RandomSeed.x, sample)
    assert.ok(Number.isFinite(sample) && sample >= 0 && sample < 1)
  })
}

test("resetting only seed replays the trace independently of stale x", () => {
  // Given: a used generator with an externally replaced last sample.
  const { RandomSeed } = loadBundle("layout-base")
  Array.from({ length: 11 }, () => RandomSeed.nextDouble())
  RandomSeed.seed = 1
  RandomSeed.x = NaN
  // When
  const samples = Array.from({ length: 6 }, () => RandomSeed.nextDouble())
  // Then
  assert.deepEqual(samples, [1015568748, 1586005467, 2165703038,
    3027450565, 217083232, 1587069247].map(state => state / modulus))
})

test("4096 draws advance exact uint32 state and stay finite in [0, 1)", () => {
  // Given
  const { RandomSeed } = loadBundle("layout-base")
  RandomSeed.seed = 4294967295
  // When
  const trace = Array.from({ length: 4096 }, () => {
    const sample = RandomSeed.nextDouble()
    return { seed: RandomSeed.seed, x: RandomSeed.x, sample }
  })
  // Then: an independent arbitrary-precision recurrence checks every advance.
  let expected = 4294967295n
  for (const { seed, x, sample } of trace) {
    expected = (1664525n * expected + 1013904223n) % (2n ** 32n)
    assert.equal(seed, Number(expected))
    assert.ok(Number.isInteger(seed) && seed >= 0 && seed < modulus)
    assert.ok(Number.isFinite(sample) && sample >= 0 && sample < 1)
    assert.equal(sample, Number(expected) / modulus)
    assert.equal(x, sample)
  }
})

test("browser-global UMD loads the full bundle and exposes the same default PRNG", () => {
  // Given
  const file = new URL("layout-base.js", vendor)
  const context = {}
  // When
  runInNewContext(readFileSync(file, "utf8"), context, { filename: file.pathname, timeout: 1000 })
  // Then
  const library = context.layoutBase
  assert.equal(typeof library, "function")
  assert.deepEqual(Object.keys(library).sort(), exportsExpected)
  assert.equal(library.RandomSeed.seed, 1)
  assert.equal(library.RandomSeed.nextDouble(), 1015568748 / modulus)
})

test("CoSE graph positioning uses the bundled PRNG golden coordinate trace", () => {
  // Given: real bundles, graph manager, root graph, and three nodes.
  const library = loadBundle("layout-base")
  const cose = loadBundle("cose-base", { "layout-base": library })
  const layout = new cose.CoSELayout()
  const graph = layout.newGraphManager().addRoot()
  const nodes = Array.from({ length: 3 }, () => graph.add(layout.newNode()))
  // When
  layout.positionNodesRandomly()
  // Then: fixed coordinates from seed 1, center (1200, 900), and +/-1000 bounds.
  assert.deepEqual(nodes.map(node => [node.getLocation().x, node.getLocation().y]), [
    [672.911050543189, 638.5413474403322],
    [1208.4840646013618, 1309.7665273584425],
    [301.0872572660446, 639.0367086045444],
  ])
  assert.equal(library.RandomSeed.seed, 1587069247)
})

test("fCoSE bundle still loads and registers through real CoSE/layout-base exports", () => {
  // Given
  const library = loadBundle("layout-base")
  const cose = loadBundle("cose-base", { "layout-base": library })
  const register = loadBundle("cytoscape-fcose", { "cose-base": cose })
  const registrations = []
  // When
  register((type, name, implementation) => registrations.push({ type, name, implementation }))
  // Then
  assert.equal(cose.layoutBase, library)
  assert.equal(registrations.length, 1)
  assert.equal(registrations[0].type, "layout")
  assert.equal(registrations[0].name, "fcose")
  assert.equal(typeof registrations[0].implementation.prototype.run, "function")
})
