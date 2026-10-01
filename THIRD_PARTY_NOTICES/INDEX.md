# Version-pinned distribution notices

These are unmodified LICENSE files extracted from the specified public npm and
crates.io archives, plus the complete unmodified `option-ext@0.2.0` source crate.
`LICENSE` at the distribution root remains RTRT's own MIT license. This directory
accompanies the source, the five GitHub binary archives, the five dashboard
platform npm packages, and `rtrt-agent`. It is a targeted notice set, not an
exhaustive dependency inventory or a legal certification.

The five MIT JS bundles `cytoscape-fcose`, `cytoscape-cola`, `cose-base`,
`layout-base`, and `webcola` are embedded in the dashboard without their upstream
LICENSE files. `cytoscape@3.30.2` already has an inline MIT header; its upstream
LICENSE is also supplied here. The Rust notices cover identified linked TLS and
other transitive dependencies; applicability to a particular optimized binary
and completeness of obligations require qualified legal review.

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
| ring@0.17.14 | https://static.crates.io/crates/ring/ring-0.17.14.crate | `a4689e6c2294d81e88dc6261c768b63bc4fcdb852be6d1352498b114f61383b7` |
| subtle@2.6.1 | https://static.crates.io/crates/subtle/subtle-2.6.1.crate | `13c2bddecc57b384dee18652358fb23172facb8a2c51ccc10d74c157bdea3292` |
| webpki-roots@0.26.11 | https://static.crates.io/crates/webpki-roots/webpki-roots-0.26.11.crate | `521bc38abb08001b01866da9f51eb7c5d647a19260e00054a8c7fd5f9e57f7a9` |
| webpki-roots@1.0.7 | https://static.crates.io/crates/webpki-roots/webpki-roots-1.0.7.crate | `52f5ee44c96cf55f1b349600768e3ece3a8f26010c05265ab73f945bb1a2eb9d` |
| option-ext@0.2.0 | https://static.crates.io/crates/option-ext/option-ext-0.2.0.crate | `04744f49eae99ab78e0d5c0b603ab218f515ea8cfe5a456d7629ad883a3b6e7d` |

Paths below are relative to this directory; SHA-256 is of the distributed local
file. LICENSE rows hash extracted files, not their upstream archives. For npm,
the upstream path is `package/LICENSE`. For crates, the upstream license path
is `<name>-<version>/<path below>`. The `SOURCE.crate` row instead hashes the
unmodified complete upstream archive bytes (also listed in the archive table).

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
