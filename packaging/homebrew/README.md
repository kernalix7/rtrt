# Homebrew tap for RTRT

This directory hosts the formula in-source. To publish it as a real tap:

1. Create a separate GitHub repo named `homebrew-tap` under the same owner
   (e.g. `kernalix7/homebrew-tap`).
2. Copy `rtrt.rb` into `Formula/rtrt.rb` in that repo and commit.
3. End users install with:

       brew tap kernalix7/tap
       brew install rtrt

When cutting a release in this repo:

1. Push `vX.Y.Z` + `REL-vX.Y.Z` tags. `.github/workflows/release.yml`
   builds the per-platform tarballs.
2. Compute the source-tarball SHA256:

       curl -L https://github.com/kernalix7/rtrt/archive/refs/tags/vX.Y.Z.tar.gz \
         | sha256sum | awk '{print $1}'

3. Update `url`, `sha256`, and `version` in `rtrt.rb` and the matching
   `Formula/rtrt.rb` in the tap repo. PR + merge in the tap repo.

The release workflow does not write to the tap repo automatically — the tap
is a user-controlled artefact and `GITHUB_TOKEN` would need elevated scope.
