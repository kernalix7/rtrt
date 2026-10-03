# Homebrew tap for RTRT

This directory hosts the formula in-source. To publish it as a real tap:

> **0.2.1 note:** Homebrew is not an automated release channel. The in-source formula's all-zero checksum is still a template; do not publish or install it until the real source-tarball checksum is available and a separate tap PR is merged. The paired-tag workflow publishes versioned npm packages and GitHub binary archives, while workspace crates remain source-only and are not on crates.io. A source version marker is not proof that any release channel is published.
>
> The five `rtrt-dashboard-*@0.1.6` npm packages that were published during the v0.1.6 attempt do not make Homebrew installable: there is no `rtrt-agent@0.1.6` on the npm registry, no GitHub Release for `v0.1.6`, and this formula's placeholder checksum is unchanged. Treat the formula above as a template only; never present it as installable.

1. Create a separate GitHub repo named `homebrew-tap` under the same owner
   (e.g. `kernalix7/homebrew-tap`).
2. Copy `rtrt.rb` into `Formula/rtrt.rb` in that repo and commit.
3. End users install with:

       brew tap kernalix7/tap
       brew install rtrt

When cutting a release in this repo:

1. After the release PR is merged, check out the merged `main` commit and create
   both tags on that commit. Push them together with `git push --atomic origin
   vX.Y.Z REL-vX.Y.Z`. `.github/workflows/release.yml` builds the per-platform
   tarballs.
2. Compute the source-tarball SHA256:

       curl -L https://github.com/kernalix7/rtrt/archive/refs/tags/vX.Y.Z.tar.gz \
         | sha256sum | awk '{print $1}'

3. Update `url`, `sha256`, and `version` in `rtrt.rb` and the matching
   `Formula/rtrt.rb` in the tap repo. PR + merge in the tap repo.

The release workflow does not write to the tap repo automatically — the tap
is a user-controlled artefact and `GITHUB_TOKEN` would need elevated scope.
