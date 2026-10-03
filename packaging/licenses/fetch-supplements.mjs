// Rehydrate only the byte-pinned full-text supplements; the release gate stays offline.
import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import { mkdir, readFile, writeFile } from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"

const notices = fileURLToPath(new URL("../../THIRD_PARTY_NOTICES/", import.meta.url))
assert.deepEqual(process.argv.slice(2), ["--write"], "usage: node packaging/licenses/fetch-supplements.mjs --write")
const { components } = JSON.parse(await readFile(path.join(notices, "SUPPLEMENTS.json"), "utf8"))
for (const entry of components) {
  for (const file of entry.files) {
    assert.ok(file.path.startsWith(`${entry.name}@${entry.version}/`) && !file.path.includes("\\") &&
      !file.path.split("/").includes(".."), `invalid destination ${file.path}`)
    assert.ok(file.url.startsWith("https://raw.githubusercontent.com/") ||
      file.url === "https://www.apache.org/licenses/LICENSE-2.0.txt", `invalid source ${file.url}`)
    const response = await fetch(file.url, { signal: AbortSignal.timeout(30_000) })
    assert.ok(response.ok, `upstream fetch failed: ${file.url} (${response.status})`)
    const bytes = Buffer.from(await response.arrayBuffer())
    assert.equal(createHash("sha256").update(bytes).digest("hex"), file.sha256,
      `upstream bytes do not match pin: ${file.url}`)
    const destination = path.join(notices, file.path)
    await mkdir(path.dirname(destination), { recursive: true })
    await writeFile(destination, bytes)
  }
}
