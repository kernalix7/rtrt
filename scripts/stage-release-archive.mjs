import { writeFile } from 'node:fs/promises'
import { pathToFileURL } from 'node:url'

export async function stageArchiveReadme(version, destination) {
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)) {
    throw new Error('invalid release version')
  }
  const base = `https://github.com/kernalix7/rtrt/blob/v${version}`
  const readme = `# rtrt v${version} — binary archive

This archive contains the three platform binaries, license, changelog, and third-party notices. Documentation is in the version-pinned source tree, not this archive.

- [Install and verify](${base}/docs/INSTALL.md)
- [Usage](${base}/docs/USAGE.md)
- [Features](${base}/docs/FEATURES.md)
- [Architecture](${base}/docs/ARCHITECTURE.md)
- [Design](${base}/DESIGN.md)
- [Security](${base}/SECURITY.md)
- [Third-party licenses](${base}/THIRD_PARTY_LICENSES.md)
- [한국어](${base}/docs/README.ko.md)
`
  await writeFile(destination, readme)
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    if (process.argv.length !== 4) throw new Error('usage: stage-release-archive.mjs <version> <destination>')
    await stageArchiveReadme(process.argv[2], process.argv[3])
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error))
    process.exitCode = 1
  }
}
