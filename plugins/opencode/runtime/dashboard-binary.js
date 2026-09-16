import { lstat, readFile } from "node:fs/promises"
import path from "node:path"
import { fileURLToPath } from "node:url"
import { readRegular, realPath, unavailable } from "./dashboard-files.js"

const PACKAGES = new Map([
  ["linux-x64", "rtrt-dashboard-linux-x64"],
  ["linux-arm64", "rtrt-dashboard-linux-arm64"],
  ["darwin-x64", "rtrt-dashboard-darwin-x64"],
  ["darwin-arm64", "rtrt-dashboard-darwin-arm64"],
  ["win32-x64", "rtrt-dashboard-win32-x64"],
])

// Node's package search layout, anchored to this plugin, without NODE_PATH,
// cwd, PATH, Cargo target directories, or runtime network downloads.
function packageRoots() {
  const roots = []
  let directory = fileURLToPath(new URL("../", import.meta.url))
  for (;;) {
    if (path.basename(directory) !== "node_modules") roots.push(path.join(directory, "node_modules"))
    const parent = path.dirname(directory)
    if (parent === directory) return roots
    directory = parent
  }
}

export async function resolveDashboardBinary({
  home, platform = process.platform, arch = process.arch,
  version, roots = packageRoots(), uid = process.geteuid?.(),
}) {
  const name = PACKAGES.get(`${platform}-${arch}`)
  if (!name) throw unavailable()
  const expected = version ?? JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8")).version
  if (typeof expected !== "string" || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(expected)) throw unavailable()
  const validate = (stat) => {
    if (stat.isSymbolicLink()) throw unavailable()
    if (platform !== "win32" && ((stat.uid !== uid && stat.uid !== 0) || (stat.mode & 0o7022) !== 0)) throw unavailable()
  }
  const cache = path.join(home, ".rtrt", "dashboard", "packages", expected, "node_modules")
  for (const root of [...roots, cache]) {
    const directory = path.join(root, name)
    try {
      await realPath(directory)
      // Check ancestors too: a private executable in a writable parent is not trusted.
      let ancestor = directory
      for (;;) {
        const stat = await lstat(ancestor)
        if (!stat.isDirectory()) throw unavailable()
        // System-owned sticky temp roots are safe when all descendants are checked.
        if (!(stat.uid === 0 && (stat.mode & 0o1777) === 0o1777)) validate(stat)
        const parent = path.dirname(ancestor)
        if (parent === ancestor) break
        ancestor = parent
      }
      const manifest = JSON.parse(await readRegular(path.join(directory, "package.json"), validate))
      if (manifest.name !== name || manifest.version !== expected) continue
      const binary = path.join(directory, "bin", platform === "win32" ? "rtrt-dashboard.exe" : "rtrt-dashboard")
      await realPath(binary)
      validate(await lstat(path.dirname(binary)))
      const stat = await lstat(binary)
      validate(stat)
      if (!stat.isFile() || stat.nlink !== 1 || (platform !== "win32" && (stat.mode & 0o111) === 0)) throw unavailable()
      return binary
    } catch (error) {
      if (error.code !== "ENOENT") throw unavailable()
    }
  }
  throw unavailable()
}
