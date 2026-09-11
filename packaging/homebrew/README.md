# Homebrew tap for RTRT

This directory hosts the formula in-source. To publish it as a real tap:

> **0.1.1 note:** Homebrew is not an automated 0.1.1 release channel. The in-source formula's all-zero checksum is a post-tag template; do not publish or install it until the real checksum is available and the separate tap PR has been merged.

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
