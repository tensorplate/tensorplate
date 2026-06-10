#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Build and sign the TensorPlate APT repository tree from verified release
# assets. The output directory is a complete static repository (pool/ +
# dists/) ready to sync to the object storage behind
# https://packages.tensorplate.com/apt; syncing/CDN policy is owned by the
# calling workflow so this script stays provider-agnostic and testable.
#
# Fail-closed properties:
#   - assets must match SHA256SUMS, and every .deb present must be listed
#   - the cosign bundle for SHA256SUMS is verified unless explicitly waived
#   - generated InRelease/Release.gpg must verify against the public
#     keyring shipped by tensorplate-apt-source (or --verify-keyring),
#     so metadata that installed hosts cannot validate never leaves CI
#   - a pool package name may never change contents (same name, new sha
#     dies; rebuilds must bump the version instead)

set -Eeuo pipefail

usage() {
  cat <<'EOF'
Usage:
  publish-apt-repo.sh --assets-dir DIR --output DIR --signing-key FILE [options]

Required:
  --assets-dir DIR       Release assets: .deb files + SHA256SUMS
                         (+ SHA256SUMS.cosign.bundle on the release path).
  --output DIR           Output directory for the repository tree.
  --signing-key FILE     Armored OpenPGP private key for metadata signing.

Options:
  --existing-pool DIR    pool/ from the previous publication; its packages
                         are carried forward so the stable channel keeps
                         serving older versions.
  --verify-keyring FILE  Armored public key the signed metadata must verify
                         against. Defaults to the keyring shipped by
                         tensorplate-apt-source
                         (packaging/apt/tensorplate-archive-keyring.asc).
  --suite NAME           Suite/codename. Defaults to jammy.
  --component NAME       Component. Defaults to main.
  --architectures "A B"  Binary architectures. Defaults to "arm64 amd64".
  --cosign-identity STR  Expected certificate identity when verifying
                         SHA256SUMS.cosign.bundle.
  --allow-unverified-assets
                         Skip cosign bundle verification. Staging and
                         container tests only; never for production.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

ASSETS_DIR=""
OUTPUT=""
SIGNING_KEY=""
EXISTING_POOL=""
VERIFY_KEYRING=""
SUITE="jammy"
COMPONENT="main"
ARCHITECTURES="arm64 amd64"
COSIGN_IDENTITY=""
ALLOW_UNVERIFIED=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --assets-dir) ASSETS_DIR="${2:-}"; shift 2 ;;
    --output) OUTPUT="${2:-}"; shift 2 ;;
    --signing-key) SIGNING_KEY="${2:-}"; shift 2 ;;
    --existing-pool) EXISTING_POOL="${2:-}"; shift 2 ;;
    --verify-keyring) VERIFY_KEYRING="${2:-}"; shift 2 ;;
    --suite) SUITE="${2:-}"; shift 2 ;;
    --component) COMPONENT="${2:-}"; shift 2 ;;
    --architectures) ARCHITECTURES="${2:-}"; shift 2 ;;
    --cosign-identity) COSIGN_IDENTITY="${2:-}"; shift 2 ;;
    --allow-unverified-assets) ALLOW_UNVERIFIED=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown option '$1'" ;;
  esac
done

[[ -n "$ASSETS_DIR" ]] || die "--assets-dir is required"
[[ -n "$OUTPUT" ]] || die "--output is required"
[[ -n "$SIGNING_KEY" ]] || die "--signing-key is required"
[[ -d "$ASSETS_DIR" ]] || die "asset directory not found: $ASSETS_DIR"
[[ -f "$SIGNING_KEY" ]] || die "signing key not found: $SIGNING_KEY"
[[ -z "$EXISTING_POOL" || -d "$EXISTING_POOL" ]] || die "existing pool not found: $EXISTING_POOL"
[[ -n "$SUITE" && -n "$COMPONENT" && -n "$ARCHITECTURES" ]] ||
  die "--suite, --component, and --architectures must not be empty"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "$VERIFY_KEYRING" ]]; then
  [[ -n "$repo_root" ]] || die "not inside a git repository; pass --verify-keyring explicitly"
  VERIFY_KEYRING="${repo_root}/packaging/apt/tensorplate-archive-keyring.asc"
fi
[[ -f "$VERIFY_KEYRING" ]] || die "verify keyring not found: $VERIFY_KEYRING"

for tool in gpg gpgv dpkg-scanpackages apt-ftparchive sha256sum gzip; do
  command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done

checksums="${ASSETS_DIR}/SHA256SUMS"
[[ -f "$checksums" ]] || die "missing ${checksums}; refusing to publish unverified assets"

note "verifying asset checksums"
(cd "$ASSETS_DIR" && sha256sum --check --ignore-missing --strict SHA256SUMS >/dev/null) ||
  die "asset checksum verification failed against $checksums"
while IFS= read -r deb; do
  deb_name="$(basename -- "$deb")"
  grep -Eq "[0-9a-f]{64}  ${deb_name//./\\.}\$" "$checksums" ||
    die "asset ${deb_name} is not covered by SHA256SUMS; refusing to publish it"
done < <(find "$ASSETS_DIR" -maxdepth 1 -type f -name '*.deb')

bundle="${checksums}.cosign.bundle"
if ((ALLOW_UNVERIFIED)); then
  note "WARNING: cosign bundle verification waived (--allow-unverified-assets); staging/test use only"
else
  [[ -f "$bundle" ]] || die "missing ${bundle}; pass --allow-unverified-assets only for staging/test publication"
  command -v cosign >/dev/null 2>&1 || die "cosign is required to verify $bundle"
  [[ -n "$COSIGN_IDENTITY" ]] || die "--cosign-identity is required to verify $bundle"
  note "verifying SHA256SUMS cosign bundle"
  cosign verify-blob \
    --bundle "$bundle" \
    --certificate-identity "$COSIGN_IDENTITY" \
    --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
    "$checksums" >/dev/null 2>&1 ||
    die "cosign verification failed for $checksums"
fi

note "staging package pool"
pool_dir="${OUTPUT}/pool/${COMPONENT}"
dists_dir="${OUTPUT}/dists/${SUITE}"
mkdir -p "$pool_dir"

stage_deb() {
  local src="$1" name dst
  name="$(basename -- "$src")"
  dst="${pool_dir}/${name}"
  if [[ -f "$dst" ]]; then
    if [[ "$(sha256sum < "$src")" == "$(sha256sum < "$dst")" ]]; then
      return 0
    fi
    die "pool already contains ${name} with different contents; bump the package version instead of republishing changed bytes"
  fi
  cp -- "$src" "$dst"
}

staged=0
if [[ -n "$EXISTING_POOL" ]]; then
  while IFS= read -r deb; do
    stage_deb "$deb"
    staged=$((staged + 1))
  done < <(find "$EXISTING_POOL" -type f -name '*.deb')
  note "carried ${staged} package(s) forward from the existing pool"
fi
new_count=0
while IFS= read -r deb; do
  stage_deb "$deb"
  new_count=$((new_count + 1))
done < <(find "$ASSETS_DIR" -maxdepth 1 -type f -name '*.deb')
((new_count > 0)) || die "no .deb assets found in $ASSETS_DIR"
note "staged ${new_count} package(s) from release assets"

note "generating package indexes"
for arch in $ARCHITECTURES; do
  arch_dir="${dists_dir}/${COMPONENT}/binary-${arch}"
  mkdir -p "$arch_dir"
  (cd "$OUTPUT" && dpkg-scanpackages --multiversion --arch "$arch" "pool/${COMPONENT}" > "dists/${SUITE}/${COMPONENT}/binary-${arch}/Packages" 2>/dev/null)
  [[ -s "${arch_dir}/Packages" ]] ||
    die "no packages indexed for ${arch}; the stable channel must serve every published architecture"
  gzip -9 -n -k -f "${arch_dir}/Packages"
done

note "generating Release metadata"
apt-ftparchive \
  -o "APT::FTPArchive::Release::Origin=TensorPlate" \
  -o "APT::FTPArchive::Release::Label=TensorPlate" \
  -o "APT::FTPArchive::Release::Suite=${SUITE}" \
  -o "APT::FTPArchive::Release::Codename=${SUITE}" \
  -o "APT::FTPArchive::Release::Components=${COMPONENT}" \
  -o "APT::FTPArchive::Release::Architectures=${ARCHITECTURES}" \
  release "$dists_dir" > "${dists_dir}/Release"

note "signing Release metadata"
gnupg_home="$(mktemp -d)"
trap 'rm -rf "$gnupg_home"' EXIT
chmod 700 "$gnupg_home"
gpg_sign=(env GNUPGHOME="$gnupg_home" gpg --batch --yes --pinentry-mode loopback --digest-algo SHA256)
if [[ -n "${TP_APT_SIGNING_KEY_PASSPHRASE:-}" ]]; then
  gpg_sign+=(--passphrase "$TP_APT_SIGNING_KEY_PASSPHRASE")
else
  gpg_sign+=(--passphrase '')
fi
env GNUPGHOME="$gnupg_home" gpg --batch --quiet --import "$SIGNING_KEY" 2>/dev/null ||
  die "could not import signing key from $SIGNING_KEY"
"${gpg_sign[@]}" --clearsign -o "${dists_dir}/InRelease" "${dists_dir}/Release" ||
  die "failed to produce InRelease"
"${gpg_sign[@]}" --armor --detach-sign -o "${dists_dir}/Release.gpg" "${dists_dir}/Release" ||
  die "failed to produce Release.gpg"

note "verifying signed metadata against the shipped bootstrap keyring"
verify_keyring_gpg="${gnupg_home}/verify-keyring.gpg"
env GNUPGHOME="$gnupg_home" gpg --batch --yes --dearmor \
  --output "$verify_keyring_gpg" "$VERIFY_KEYRING" ||
  die "could not dearmor $VERIFY_KEYRING"
gpgv --keyring "$verify_keyring_gpg" "${dists_dir}/InRelease" >/dev/null 2>&1 ||
  die "InRelease does not verify against ${VERIFY_KEYRING}; installed hosts could not validate this repository (wrong signing key?)"
gpgv --keyring "$verify_keyring_gpg" "${dists_dir}/Release.gpg" "${dists_dir}/Release" >/dev/null 2>&1 ||
  die "Release.gpg does not verify against ${VERIFY_KEYRING}"

note "checking repository structure"
for arch in $ARCHITECTURES; do
  grep -q "${COMPONENT}/binary-${arch}/Packages" "${dists_dir}/Release" ||
    die "Release metadata does not cover ${COMPONENT}/binary-${arch}/Packages"
done
while IFS= read -r filename; do
  [[ -f "${OUTPUT}/${filename}" ]] ||
    die "Packages index references missing pool file: ${filename}"
done < <(awk '/^Filename: /{print $2}' "${dists_dir}/${COMPONENT}"/binary-*/Packages | sort -u)

total="$(find "${OUTPUT}/pool" -type f -name '*.deb' | wc -l | tr -d ' ')"
note "repository ready at ${OUTPUT} (${total} pooled package file(s), suite ${SUITE}, architectures: ${ARCHITECTURES})"
note "sync order matters: upload pool/ before dists/ so metadata never references missing packages"
