#!/usr/bin/env bash
#
# Cut a release of Photo Viewer Classic.
#
#   scripts/release.sh           Bump the patch version (the C in A.B.C) and release.
#   scripts/release.sh 0.2.0     Release an explicit version (no auto-bump).
#
# What it does, in order:
#   1. Sanity-check: on `main`, clean tree, tag doesn't already exist.
#   2. Set the single [workspace.package] version (bump patch, or use the arg).
#   3. Commit the bump (skipped if the version is unchanged) and create tag vX.Y.Z.
#   4. Verify the *Linux* leg of the release workflow locally with `act`. The final
#      "Publish GitHub Release" step FAILS under act (no GitHub API) — that is expected
#      and fine; we only require the build + package steps to pass.
#   5. Push main and the tag, which triggers the real cross-platform release build.
#
# If the act build check fails, the local tag is removed and nothing is pushed.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
ROOT_CARGO="Cargo.toml"
WORKFLOW=".github/workflows/release.yml"

die() { echo "error: $*" >&2; exit 1; }

# --- preconditions ---------------------------------------------------------
command -v act >/dev/null || die "'act' not found — needed to verify the release workflow locally"
[ -f "$WORKFLOW" ] || die "$WORKFLOW not found"

branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] || die "releases are cut from 'main' (currently on '$branch')"
git diff --quiet && git diff --cached --quiet || die "working tree is dirty — commit or stash first"

# --- determine the target version ------------------------------------------
current="$(grep -m1 '^version = ' "$ROOT_CARGO" | sed -E 's/.*"([0-9]+\.[0-9]+\.[0-9]+)".*/\1/')"
[ -n "$current" ] || die "could not read [workspace.package] version from $ROOT_CARGO"

if [ "$#" -ge 1 ]; then
  target="$1"
else
  IFS=. read -r major minor patch <<<"$current"
  target="$major.$minor.$((patch + 1))"
fi
echo "$target" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "invalid version '$target' (want A.B.C)"
tag="v$target"

git rev-parse -q --verify "refs/tags/$tag" >/dev/null 2>&1 && die "tag $tag already exists"

echo "==> Releasing $tag  (current workspace version: $current)"

# --- bump the version (only if it changed) ---------------------------------
if [ "$target" != "$current" ]; then
  perl -0pi -e "s/(\[workspace\.package\]\nversion = \")[0-9]+\.[0-9]+\.[0-9]+(\")/\${1}${target}\${2}/" "$ROOT_CARGO"
  grep -q "version = \"$target\"" "$ROOT_CARGO" || die "version bump did not apply — check $ROOT_CARGO"
  cargo update --workspace >/dev/null 2>&1 || true # refresh the member versions in Cargo.lock
  git add "$ROOT_CARGO" Cargo.lock
  git commit -m "release: $tag"
  echo "==> Bumped workspace version $current -> $target"
else
  echo "==> Workspace version is already $target — tagging current HEAD (no bump commit)"
fi

# --- create the tag --------------------------------------------------------
git tag -a "$tag" -m "Release $tag"
echo "==> Created tag $tag"

# --- verify the Linux release build with act -------------------------------
echo "==> Verifying the Linux release build with act (the publish step is expected to fail under act)…"
event="$(mktemp)"
log="$(mktemp)"
printf '{"ref":"refs/tags/%s"}\n' "$tag" >"$event"

set +e
act push \
  --workflows "$WORKFLOW" \
  --job build \
  --matrix "os:ubuntu-latest" \
  --eventpath "$event" 2>&1 | tee "$log"
set -e
rm -f "$event"

if grep -Eq 'Success.*Build release binary' "$log" && ! grep -Eq 'Failure.*Build release binary' "$log"; then
  echo "==> act: the Linux release binary built successfully (a failing publish step under act is expected)."
  rm -f "$log"
else
  rm -f "$log"
  git tag -d "$tag" >/dev/null
  die "the Linux release build did NOT succeed under act (see the log above). Aborted; removed local tag $tag."
fi

# --- push to trigger the real release --------------------------------------
echo "==> Pushing $branch and tag $tag to trigger the GitHub release build…"
git push origin "$branch"
git push origin "$tag"

echo "==> Done. Watch the build with:"
echo "      gh run watch \"\$(gh run list --workflow Release --limit 1 --json databaseId --jq '.[0].databaseId')\""
