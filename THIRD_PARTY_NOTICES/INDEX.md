# Version-pinned distribution notices

These include unmodified LICENSE/LICENCE/COPYING/COPYRIGHT/NOTICE files extracted
recursively from checksum-pinned registry crates, original full files carrying
`ring@0.17.14` source-header grants, the public npm LICENSE files listed below,
and the complete unmodified `option-ext@0.2.0` source crate.
`LICENSE` at the distribution root remains RTRT's own MIT license. This directory
accompanies the source, the five GitHub binary archives, the five dashboard
platform npm packages, and `rtrt-agent`. `INVENTORY.json` maps the exact three
product roots × five release targets' default-feature normal/build Cargo graphs
to package name, version, lockfile/archive SHA-256, declared SPDX expression,
upstream archive URL, extracted path and local file SHA-256. It is a deliberately
conservative graph union, **not** a retained-code or optimized-binary SBOM and
not a legal certification. This index's original digest table remains the
historical selected subset; the inventory maps all additional Rust files.

The directory contains 732 files. Ring contributes 247: the union of 244 pinned
source-header paths and six original notice paths, with three overlaps. These
complete files preserve the original bytes from the checksum-pinned archive.
The collector rejects ring versions other than `0.17.14`. The existing
eight supplement entries and nine full-text supplements are unchanged.

This corpus covers the default-feature Rust release graphs and the listed JS
notices. Optional Cargo features, downloaded models, installed npm dependency
trees, and Docker base/OS packages require separate license and notice review.
The current Dockerfile copies only RTRT's LICENSE and README into the image;
it does not copy this notice corpus.

The JS bundles `cytoscape-fcose`, `cytoscape-cola`, `cose-base`, `layout-base`,
and `webcola` are embedded in the dashboard without their upstream
LICENSE files. `cytoscape@3.30.2` already has an inline MIT header; its upstream
LICENSE is also supplied here. `layout-base` additionally embeds a JamaJS-derived
Apache-2.0 SVD subpart; see the separate readable extracted attribution below.
Applicability to optimized binaries and legal sufficiency require qualified review.

Each archive URL below is version-specific. npm tarballs were checked against
the matching `https://registry.npmjs.org/<name>/<version>` `dist.integrity`
(SHA-512 SRI); crates were checked against the matching version record in
`https://index.crates.io/` (`cksum`, SHA-256) and `Cargo.lock`.

| Component | Immutable archive URL | Archive SHA-256 |
|---|---|---|
| cytoscape-fcose@2.2.0 | https://registry.npmjs.org/cytoscape-fcose/-/cytoscape-fcose-2.2.0.tgz | `4e1f30e1821fdd4b66f21043c8ea40a8e2ad0ba75e4ccdd2c24d289cf5595432` |
| cytoscape-cola@2.5.1 | https://registry.npmjs.org/cytoscape-cola/-/cytoscape-cola-2.5.1.tgz | `ffe35cc4a027169755e571efae9d3ffe07080fe8a47321319a35788d1bda71bc` |
| cose-base@2.2.0 | https://registry.npmjs.org/cose-base/-/cose-base-2.2.0.tgz | `612ec91a30b7f2dc158cca4846eeea1b7f89a97fc672951da3c1932107de6da8` |
| layout-base@2.0.1 | https://registry.npmjs.org/layout-base/-/layout-base-2.0.1.tgz | `34165e46d8c4b9d719a592e39a60fa5f7324c21a3edd02733e1116b17013defd` |
| webcola@3.4.0 | https://registry.npmjs.org/webcola/-/webcola-3.4.0.tgz | `94ea191c50624c05d7f9be7231a43c06021a54991c82dd7ed0ff907918f899cd` |
| cytoscape@3.30.2 | https://registry.npmjs.org/cytoscape/-/cytoscape-3.30.2.tgz | `8e9bfaaf9f5f461642355b0d17c6cf1d0d58c5cb9e00ac983e2d242fd1cfb928` |
| tree-sitter-javascript@0.23.1 | https://registry.npmjs.org/tree-sitter-javascript/-/tree-sitter-javascript-0.23.1.tgz | `90e80b25a67517a4daf6ad751557bee21efbda7b7a5a554897933245d1734398` |
| ring@0.17.14 | https://static.crates.io/crates/ring/ring-0.17.14.crate | `a4689e6c2294d81e88dc6261c768b63bc4fcdb852be6d1352498b114f61383b7` |
| subtle@2.6.1 | https://static.crates.io/crates/subtle/subtle-2.6.1.crate | `13c2bddecc57b384dee18652358fb23172facb8a2c51ccc10d74c157bdea3292` |
| webpki-roots@0.26.11 | https://static.crates.io/crates/webpki-roots/webpki-roots-0.26.11.crate | `521bc38abb08001b01866da9f51eb7c5d647a19260e00054a8c7fd5f9e57f7a9` |
| webpki-roots@1.0.7 | https://static.crates.io/crates/webpki-roots/webpki-roots-1.0.7.crate | `52f5ee44c96cf55f1b349600768e3ece3a8f26010c05265ab73f945bb1a2eb9d` |
| option-ext@0.2.0 | https://static.crates.io/crates/option-ext/option-ext-0.2.0.crate | `04744f49eae99ab78e0d5c0b603ab218f515ea8cfe5a456d7629ad883a3b6e7d` |
| matchit@0.8.4 | https://static.crates.io/crates/matchit/matchit-0.8.4.crate | `47e1ffaa40ddd1f3ed91f717a33c8c0ee23fff369e3aa8772b9605cc1d22f4c3` |

Paths below are relative to this directory; SHA-256 is of the distributed local
file. LICENSE rows hash extracted files, not their upstream archives. For npm,
the upstream path is `package/LICENSE`. For crates, the upstream license path
is `<name>-<version>/<path below>`. The `SOURCE.crate` row instead hashes the
unmodified complete upstream archive bytes (also listed in the archive table).
`JamaJS-APACHE.txt` is the exact embedded bundle excerpt, not a top-level npm
LICENSE file.

| Extracted file | SHA-256 |
|---|---|
| `cytoscape-fcose@2.2.0/LICENSE` | `2837634f403949215760fcdd2fa1ed0c64875d02099ecc8318c704b852f1421d` |
| `cytoscape-cola@2.5.1/LICENSE` | `178b068f0efd88dea464c9bc8af2c58bfce59baff9a6df3a2fb05d86c0f1bf74` |
| `cose-base@2.2.0/LICENSE` | `5fb3cf4a14c3c5af6e473a192df8bca10c77754e3a0c6492c79fb92a76a5478a` |
| `layout-base@2.0.1/LICENSE` | `eabb762d8a95109a39c9be3247325529a5239a7aca327d909c3ccdc41f3a06bf` |
| `webcola@3.4.0/LICENSE` | `38431761c57600295cfbf8d214d1615e4dcabe1166cbefb086143ea52cf86600` |
| `cytoscape@3.30.2/LICENSE` | `a29c0d78a54de204b78357976c3d272a573b8592d0478b9db07af3f1da31a65a` |
| `ring@0.17.14/LICENSE` | `b3d734001a94efff3579978d953391aa7115f877657d25eb54037a43875d078a` |
| `ring@0.17.14/LICENSE-BoringSSL` | `005fc765ddc5115da796cca915baa9557abae13ff35e0a47c47affc56f6c414d` |
| `ring@0.17.14/LICENSE-other-bits` | `f025ccfb7dfb6bdfedc75ca0f67acc69e6fb4998143d834f7c2f38a29989680f` |
| `ring@0.17.14/src/polyfill/once_cell/LICENSE-APACHE` | `a60eea817514531668d7e00765731449fe14d059d3249e0bc93b36de45f759f2` |
| `ring@0.17.14/src/polyfill/once_cell/LICENSE-MIT` | `6ee2ed6c77710de911761acd5fc1ad1da00f476beb1a7ef27e78c2d1858deafc` |
| `ring@0.17.14/third_party/fiat/LICENSE` | `9eacbcb81be660840c714a560a9d65ba07913db98dd4baf969f78dd499fdd60f` |
| `subtle@2.6.1/LICENSE` | `d1fc1bc0d155df60b2e7705b6b2ae02a05c96f948e1cec6e2fb86360b09f346b` |
| `webpki-roots@0.26.11/LICENSE` | `e271993808fec50ab29350b39539cdec611a9103f827e0aa26d61da70e2d33f8` |
| `webpki-roots@1.0.7/LICENSE` | `e271993808fec50ab29350b39539cdec611a9103f827e0aa26d61da70e2d33f8` |
| `option-ext@0.2.0/LICENSE.txt` | `66a3107d5ad6a058aab753eaac2047ccb2ed0e39465dd0fe5844da3e300d5172` |
| `option-ext@0.2.0/SOURCE.crate` | `04744f49eae99ab78e0d5c0b603ab218f515ea8cfe5a456d7629ad883a3b6e7d` |
| `matchit@0.8.4/LICENSE` | `de701d0618d694feb1af90f02181a1763d9b0bdeb70a3a592781e529077dba65` |
| `matchit@0.8.4/LICENSE.httprouter` | `162ce11ad71338d0a0c6ebaf5c48af72c6ae237b468859d3656fe2d9ed3f3a85` |
| `layout-base@2.0.1/JamaJS-APACHE.txt` | `bcdabd6ef33e823c039c0de9a1427b96d0d4cf457449d7b786323172178cd4f4` |
| `whatlang@0.16.4/LICENSE` | `9fb62b415784b27e358a03677cac6b56de73425f4fdbb5f4ecf19650af5dfe0c` |
| `tree-sitter-python@0.23.6/LICENSE` | `d724405ce238a22c0d35769c5a36b386ad5958192efe8bbb304fb2896254575f` |
| `tree-sitter-typescript@0.23.2/LICENSE` | `49bf33cf78ef5897e4e161ce1517df7de1ae5042a65b6bcfd44401e0fc606559` |
| `tree-sitter-javascript@0.23.1/LICENSE` | `2e0110e07abef7c2548b26ec9d6969775617ca539a0dc8dbeeb14d6452c711d1` |
| `instant-distance@0.6.1/LICENSE` | `43070e2d4e532684de521b885f385d0841030efa2b1a20bafb76133a5e1379c1` |
| `rmcp@1.7.0/LICENSE` | `0382b0057770ca05e9c350a50aa3b1c1fea84da0bc81d723bf00b9aa841be58a` |
| `rmcp-macros@1.7.0/LICENSE` | `0382b0057770ca05e9c350a50aa3b1c1fea84da0bc81d723bf00b9aa841be58a` |
| `sse-stream@0.2.3/LICENSE-APACHE` | `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30` |
| `sse-stream@0.2.3/LICENSE-MIT` | `049d23c7810a0723f186a193f841378dbc307f62be89bdba21c73126b0df2c52` |
| `eventsource-stream@0.2.3/DECLARED-APACHE-2.0.txt` | `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30` |

The `matchit@0.8.4` archive SHA-256 is
`47e1ffaa40ddd1f3ed91f717a33c8c0ee23fff369e3aa8772b9605cc1d22f4c3`
(`https://static.crates.io/crates/matchit/matchit-0.8.4.crate`). Its SPDX
expression is **MIT AND BSD-3-Clause**, not OR: both the Ibraheem Ahmed MIT
`LICENSE` and Julien Schmidt httprouter BSD-3-Clause `LICENSE.httprouter`
are supplied as original bytes. The complete crate/checksum/file mapping is
also in `INVENTORY.json`. `rustls-webpki@0.103.15/LICENSE` (ISC, Brian Smith),
`tokio@1.52.3/LICENSE` (MIT, Tokio Contributors), Unicode-3.0/ICU notices and
all other release graph crate notice paths are there too. A reference from
rustls-webpki's LICENSE to `third-party/chromium/LICENSE` does not imply that
file exists in its published registry crate; the cache/archive contains no
`third-party/` subtree. Do not invent its contents.

**Modification notice — RTRT, 2026-10-08:** The vendored `layout-base@2.0.1`
bundle replaces the Stack Overflow-derived `RandomSeed` implementation, whose
rights and attribution were uncertain, with an original non-cryptographic
32-bit LCG. The API shape is preserved; seeded sequences and initial graph
positions change. The current vendor bundle is modified from upstream.

| Bundle identity | SHA-256 |
|---|---|
| Current `crates/rtrt-dashboard/ui/vendor/layout-base.js` | `7296ca281741b5eda7ec4ed626851275fa9262b37d2ae462319e84f38e1e9c7e` |
| Original upstream `layout-base@2.0.1` bundle (historical) | `ec15ab5df9af3f20708f4faab994accf91cda71848cd5bb10a23432cc50b6745` |

The embedded JamaJS attribution and complete Apache-2.0 excerpt remain unchanged
from the original upstream bundle: 11,963 bytes, SHA-256
`c969115b75246adef3cc3cc1213c2717e0959cf7283e7ced16ad2ec84a2e8d44`.
The standalone `layout-base@2.0.1/JamaJS-APACHE.txt` is also unchanged; it contains
that excerpt plus its existing final newline, with the digest listed above.
The inline attribution names `https://github.com/dragonfly-ai/JamaJS` and describes
the changes made for fcose. The npm archive has a separate SHA-256,
`34165e46d8c4b9d719a592e39a60fa5f7324c21a3edd02733e1116b17013defd`.
Registry `gitHead` is
`3f7549940feef31416cc35ef8256282ebc4d1ecd`, the commit peeled from annotated tag
`v2.0.1`. The tarball and that git tree have no `NOTICE` file. `dragonfly-ai/JamaJS`
at the inspected `master` tip also has a LICENSE and no NOTICE file. This does
**not** guarantee that no pertinent NOTICE existed in another JamaJS commit or
distribution. The SVD code is a described adaptation, not a byte-identical copy
of JamaJS. Preserve the embedded source attribution.

The additional `tree-sitter-javascript@0.23.1/LICENSE` conservatively preserves
the original MIT text, including `Copyright (c) 2014 Max Brunsfeld`, for the
JavaScript grammar inherited by `tree-sitter-typescript@0.23.2`. The official
upstream lock at the TypeScript source pin
`f975a621f4e7f532fe322e13c4f79495e0a7b2e7` resolves JavaScript `0.23.1`.
The notice comes from the npm `.tgz` listed above, not a Rust `.crate`, and is
indexed separately from the Cargo component mapping.

For `OR` expressions this inventory records the **unselected upstream SPDX
expression** and ships *all* original archive license files; it does not claim
that every branch must apply. For `AND`, both terms and available subpart files
are carried. Eight registry packages have **no LICENSE/NOTICE file in their
published crate archive**. Their original `Cargo.toml` remains byte evidence
and `upstreamNoticeFiles: 0` still means the crate archive itself had no
license file. `SUPPLEMENTS.json` now pins a full-text supplement for each one.
The supplement is not a relicensing, a compiled SBOM, or a legal certification.

The component commit is the `.cargo_vcs_info.json` git SHA inside the
checksum-pinned `.crate`, not a tag guess. Same-commit texts were retrieved
from that SHA. `whatlang@0.16.4` carries the upstream MIT text and its five
copyright lines (Potapov 2017, Wormer 2014, Johnson 2008, Rideout 2006,
Ceglowski 2004). `tree-sitter-python@0.23.6` and
`tree-sitter-typescript@0.23.2` carry their pin MIT texts (Brunsfeld 2016 and
2017). `rmcp@1.7.0` and `rmcp-macros@1.7.0` share one composite `LICENSE` from
`3529c3675ff64db805bd947ca6ece6090809e43d`: Apache-2.0 terms, an MIT grant to
Model Context Protocol / LF Projects, and a CC-BY-4.0 URL pointer. That file
does not include the CC-BY-4.0 legal code. Cargo declares Apache-2.0 only;
both original sections are kept.

`instant-distance@0.6.1` declares MIT OR Apache-2.0. The selected branch is
the original Apache-2.0 `LICENSE` at `bbdc1b19bf3cdd372199e1d697bd0d7fd9a10ebb`.
Its appendix copyright placeholder is unfilled. No MIT copyright was invented.
That file is not byte-identical to the Apache Software Foundation canonical
text. `sse-stream@0.2.3` has no license file at component commit
`70fd1a9da602060069e9da6e337a17be4f496455`. Both later `LICENSE-APACHE` and
`LICENSE-MIT` files from the same author at
`9b95874ca02cc337b9f86b54915ba6c6b27821d0` are shipped; the selected branch is
Apache-2.0. Those later files were not inside the published 0.2.3 crate.
`eventsource-stream@0.2.3` has no license file at
`3d46f1c758f9ee4681e9da0427556d24c53f9c01` or elsewhere in that repository
history. Cargo.toml and README declare MIT OR Apache-2.0. The selected branch
is Apache-2.0, shipped as `DECLARED-APACHE-2.0.txt`, the verbatim canonical
text from <https://www.apache.org/licenses/LICENSE-2.0.txt>. It is a
declared-standard supplement, not a recovered upstream file, and it does not
add a copyright line for Julian Popescu. No additional proprietary source,
optional model, or all-features graph is implied.

`ring`'s top-level LICENSE explains its ISC/Apache-2.0 BoringSSL split and
points to the included once_cell subpart texts. The included fiat subpart
license is copied from the same crate. `option-ext@0.2.0` is MPL-2.0: its
complete, unchanged upstream source archive is bundled at
`THIRD_PARTY_NOTICES/option-ext@0.2.0/SOURCE.crate` inside each of the five
GitHub binary archives, five dashboard platform npm packages, and the
`rtrt-agent` npm package. It is the same version and SHA-256 as `Cargo.lock`;
recipients need no network download. From the distribution root, run
`mkdir option-ext-source && tar -xzf THIRD_PARTY_NOTICES/option-ext@0.2.0/SOURCE.crate -C option-ext-source`
to read the original `option-ext-0.2.0/src/`, `Cargo.toml`, `Cargo.toml.orig`,
`README.md`, and `LICENSE.txt` beneath `option-ext-source/`. The URL above is
an additional upstream retrieval option, not the only source offer. This
source-code copy applies only to the MPL-covered `option-ext` component; it
does not relicense RTRT. Qualified counsel should assess any remaining
distribution obligations, including MPL-2.0 section 3.2.
