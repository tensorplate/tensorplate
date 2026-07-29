#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Open (or update) an auto-merge formula-graph pull request in the TensorPlate
# Homebrew tap for a released tag. The script computes the source-tarball
# sha256, renders every component and meta-formula from the checked-in
# templates, pushes a branch to the tap, and opens a PR with auto-merge armed
# so the tap's own CI (audit +
# build-from-source + `brew test` on Apple Silicon) gates the actual merge.
#
# Completion means "PR opened with auto-merge armed", not "merged": go-live is
# eventually consistent once the tap CI passes. The script is idempotent — a
# re-run for a version whose formula already points at the tag is a no-op.
#
# Auth: GH_TOKEN must hold a token (fine-grained PAT or GitHub App) with
# contents + pull_requests write scoped to the tap repo only. It is used for
# the git push and the PR operations; no token is written to disk.

set -Eeuo pipefail

usage() {
  cat <<'EOF'
Usage:
  publish-homebrew-formula.sh --tag vX.Y.Z [options]

Required:
  --tag vX.Y.Z          Released tag to bump the formula to.

Options:
  --source-repo OWNER/REPO  Source repository whose tag tarball is hashed.
                            Defaults to tensorplate/tensorplate.
  --tap-repo OWNER/REPO     Homebrew tap repository. Defaults to
                            tensorplate/homebrew-tap.
  --formula-path PATH       Compatibility option naming the meta-formula path
                            in the tap. The complete graph is rendered beside
                            it. Defaults to Formula/tensorplate.rb.
  --help|-h                 Show this help.

Environment:
  GH_TOKEN              Tap-scoped token (contents + pull_requests write).
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

TAG=""
SOURCE_REPO="tensorplate/tensorplate"
TAP_REPO="tensorplate/homebrew-tap"
FORMULA_PATH="Formula/tensorplate.rb"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag) TAG="${2:-}"; shift 2 ;;
    --source-repo) SOURCE_REPO="${2:-}"; shift 2 ;;
    --tap-repo) TAP_REPO="${2:-}"; shift 2 ;;
    --formula-path) FORMULA_PATH="${2:-}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown option '$1'" ;;
  esac
done

[[ -n "$TAG" ]] || die "--tag is required"
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  die "the Homebrew tap only tracks final vX.Y.Z releases; got '$TAG'"
[[ "$SOURCE_REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  die "--source-repo must be OWNER/REPO"
[[ "$TAP_REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
  die "--tap-repo must be OWNER/REPO"
[[ "$FORMULA_PATH" =~ ^[A-Za-z0-9._/-]+$ && "$FORMULA_PATH" != /* &&
  "$FORMULA_PATH" != *".."* && "$(basename "$FORMULA_PATH")" == "tensorplate.rb" ]] ||
  die "--formula-path must be a relative path ending in tensorplate.rb"
[[ -n "${GH_TOKEN:-}" ]] || die "GH_TOKEN must be set to a tap-scoped token"
for tool in gh git curl sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
renderer="${script_dir}/render-homebrew-formulas.sh"
[[ -x "$renderer" ]] || die "formula renderer is missing or not executable: $renderer"

version="${TAG#v}"
tarball_url="https://github.com/${SOURCE_REPO}/archive/refs/tags/${TAG}.tar.gz"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

note "computing source tarball sha256 for ${TAG}"
curl -fsSL "$tarball_url" -o "${workdir}/src.tar.gz" ||
  die "could not download source tarball ${tarball_url}"
sha256="$(sha256sum "${workdir}/src.tar.gz" | awk '{print $1}')"
[[ "$sha256" =~ ^[0-9a-f]{64}$ ]] || die "unexpected sha256 '${sha256}'"
note "source sha256: ${sha256}"

# gh respects GH_TOKEN for API + clone; setup-git routes git push through it.
gh auth setup-git
tap_dir="${workdir}/tap"
note "cloning ${TAP_REPO}"
gh repo clone "$TAP_REPO" "$tap_dir" -- --depth=1 >/dev/null 2>&1 ||
  die "could not clone ${TAP_REPO} (token scope?)"

formula_path="${FORMULA_PATH%/*}"
if [[ "$formula_path" == "$FORMULA_PATH" ]]; then
  formula_path="."
fi
note "rendering the complete formula graph into ${formula_path}"
"$renderer" \
  --source-url "$tarball_url" \
  --sha256 "$sha256" \
  --output-dir "${tap_dir}/${formula_path}" >/dev/null

git -C "$tap_dir" config user.name "tensorplate-release-bot"
git -C "$tap_dir" config user.email "release-bot@users.noreply.github.com"
git -C "$tap_dir" add "$formula_path"

if git -C "$tap_dir" diff --cached --quiet -- "$formula_path"; then
  note "formula graph already points at ${TAG}; nothing to publish"
  exit 0
fi

branch="bump-tensorplate-${version}"
title="tensorplate formula graph ${version}"
body="Automated component and meta-formula bump to \`${TAG}\` from the TensorPlate release pipeline.

- url: ${tarball_url}
- sha256: ${sha256}
- formulas: tensorplate-agent, tensorplate-serving, tensorplate-cli, tensorplate-observability, tensorplate-backend-python-pytorch, tensorplate

Auto-merge is armed; the tap CI (audit + build-from-source + \`brew test\` on Apple Silicon) gates the merge."

git -C "$tap_dir" checkout -b "$branch"
git -C "$tap_dir" commit -m "$title" >/dev/null
note "pushing ${branch} to ${TAP_REPO}"
# Plain force-push: this branch is owned entirely by the release automation
# and is regenerated deterministically every run, so a recovery re-run must
# overwrite a still-open bump branch. The shallow single-branch clone has no
# remote-tracking ref for it, so --force-with-lease would reject the re-run
# as stale and strand the recovery before the PR-reuse path below.
git -C "$tap_dir" push --force -u origin "$branch"

# Reuse an open PR for this branch if a prior run already opened one.
existing="$(gh pr list --repo "$TAP_REPO" --head "$branch" --state open --json number --jq '.[0].number' 2>/dev/null || true)"
if [[ -n "$existing" ]]; then
  note "updating existing PR #${existing}"
  pr_ref="$existing"
else
  note "opening formula-bump PR"
  gh pr create --repo "$TAP_REPO" --head "$branch" --title "$title" --body "$body"
  pr_ref="$branch"
fi

note "arming auto-merge"
# Auto-merge holds the PR until the tap's required status checks pass.
gh pr merge --repo "$TAP_REPO" "$pr_ref" --auto --squash ||
  die "could not arm auto-merge; enable auto-merge and a required check on ${TAP_REPO}, or merge the PR manually and rerun for confirmation"

note "homebrew formula-bump PR for ${TAG} is open with auto-merge armed"
