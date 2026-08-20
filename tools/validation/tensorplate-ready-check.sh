#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Verify that a host is "TensorPlate-ready": the TensorPlate APT source
# and archive keyring are preconfigured, so the runtime install is only
#
#   sudo apt update
#   sudo apt install tensorplate
#
# Run on a freshly provisioned image (or any host) before handing it to a
# user, and again with --online to prove the stable channel is reachable
# and trusted. Exits non-zero if the host is not ready.
#
# Fixture/test overrides (used by test/packaging/verify_ready_check.sh):
#   TP_READY_KEYRING       keyring path (default /usr/share/keyrings/...)
#   TP_READY_SOURCES       sources path (default /etc/apt/sources.list.d/...)
#   TP_READY_EXPECTED_URI  expected repository URI (default stable channel)
#   TP_READY_SKIP_DPKG=1   skip the dpkg registration check (off-host runs)

set -Eeuo pipefail

KEYRING="${TP_READY_KEYRING:-/usr/share/keyrings/tensorplate-archive-keyring.gpg}"
SOURCES="${TP_READY_SOURCES:-/etc/apt/sources.list.d/tensorplate.sources}"
EXPECTED_URI="${TP_READY_EXPECTED_URI:-https://packages.tensorplate.com/apt}"
ONLINE=0

usage() {
  cat <<'EOF'
Usage:
  tensorplate-ready-check.sh [--online]

Options:
  --online   Also run `apt-get update` against the configured source and
             require trusted, warning-free metadata plus an installable
             `tensorplate` candidate. Requires root and network access.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --online) ONLINE=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'error: unknown option %s\n' "$1" >&2; exit 1 ;;
  esac
done

fail=0
pass() { printf 'PASS: %s\n' "$*"; }
warn() { printf 'WARN: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

# --- archive keyring -------------------------------------------------------
if [[ ! -s "$KEYRING" ]]; then
  fail "archive keyring missing or empty: $KEYRING"
elif [[ "$(head -c1 "$KEYRING")" == "-" ]]; then
  fail "archive keyring at $KEYRING is ASCII-armored; APT needs the binary (dearmored) form"
else
  pass "archive keyring present: $KEYRING"
  if command -v gpg >/dev/null 2>&1; then
    if gpg --show-keys "$KEYRING" >/dev/null 2>&1; then
      pass "archive keyring is a valid OpenPGP keyring"
    else
      fail "archive keyring does not parse as an OpenPGP keyring"
    fi
  else
    warn "gpg not available; skipped keyring parse check"
  fi
fi

# --- Deb822 source ---------------------------------------------------------
if [[ ! -s "$SOURCES" ]]; then
  fail "APT source missing or empty: $SOURCES"
else
  pass "APT source present: $SOURCES"
  grep -q '^Types: deb$' "$SOURCES" ||
    fail "$SOURCES must declare 'Types: deb'"
  if grep -q "^URIs: ${EXPECTED_URI}\$" "$SOURCES"; then
    pass "repository URI is the stable channel: $EXPECTED_URI"
  else
    fail "$SOURCES URIs is not '$EXPECTED_URI' (got: $(grep '^URIs:' "$SOURCES" || echo '<none>'))"
  fi
  if grep '^URIs:' "$SOURCES" | grep -Eq '[0-9]+\.[0-9]+\.[0-9]+'; then
    fail "repository URI embeds a runtime version; the channel must be version-stable"
  fi
  signed_by="$(awk '/^Signed-By: /{print $2; exit}' "$SOURCES" || true)"
  if [[ -z "$signed_by" ]]; then
    fail "$SOURCES must pin the archive keyring via Signed-By"
  elif [[ "$signed_by" != "$KEYRING" ]]; then
    fail "Signed-By ($signed_by) does not match the expected keyring path ($KEYRING)"
  elif [[ ! -s "$signed_by" ]]; then
    fail "Signed-By points at a missing keyring: $signed_by"
  else
    pass "Signed-By pins the archive keyring"
  fi
fi

# --- bootstrap package registration ----------------------------------------
if [[ "${TP_READY_SKIP_DPKG:-0}" -eq 1 ]]; then
  warn "dpkg registration check skipped (TP_READY_SKIP_DPKG=1)"
elif ! command -v dpkg >/dev/null 2>&1; then
  warn "dpkg not available; skipped bootstrap package registration check"
elif dpkg -s tensorplate-apt-source >/dev/null 2>&1; then
  pass "tensorplate-apt-source package is installed"
else
  fail "tensorplate-apt-source is not installed; the source files are unmanaged and will not receive bootstrap updates"
fi

# --- online channel check ---------------------------------------------------
if ((ONLINE)); then
  if [[ "$(id -u)" -ne 0 ]]; then
    fail "--online requires root (apt-get update)"
  else
    update_log="$(mktemp)"
    # Bounded, because a source that stalls rather than fails would hang
    # this check indefinitely -- and a readiness check that never answers
    # is worse than one that says no. Deliberately NOT retried: whether
    # `apt-get update` succeeds against the configured sources is the
    # thing being reported, and retrying until it works would hide the
    # flakiness this exists to surface.
    update_status=0
    timeout -k 30 240 apt-get update >"$update_log" 2>&1 || update_status=$?
    if ((update_status == 0)); then
      pass "apt-get update succeeded"
    elif ((update_status == 124 || update_status == 137)); then
      fail "apt-get update did not finish within 240s; a configured source is accepting the connection but not answering"
      cat "$update_log" >&2
    else
      fail "apt-get update failed; see output below"
      cat "$update_log" >&2
    fi
    if grep -qiE 'NO_PUBKEY|is not signed|insecure repositor' "$update_log"; then
      fail "apt-get update reported trust problems for configured sources"
      grep -iE 'NO_PUBKEY|is not signed|insecure repositor' "$update_log" >&2
    else
      pass "no trust warnings from apt-get update"
    fi
    rm -f "$update_log"
    candidate="$(apt-cache policy tensorplate 2>/dev/null | awk '/Candidate:/{print $2}')"
    if [[ -n "$candidate" && "$candidate" != "(none)" ]]; then
      pass "tensorplate candidate available from the channel: $candidate"
    else
      fail "no tensorplate candidate visible; the host cannot run 'apt install tensorplate'"
    fi
  fi
fi

if ((fail)); then
  printf '\nNOT TensorPlate-ready. On a stock image, run the one-time bootstrap:\n' >&2
  printf '  sudo dpkg -i tensorplate-apt-source_<version>_all.deb   # from the GitHub Release assets\n' >&2
  exit 1
fi
printf '\nTensorPlate-ready. Runtime install is:\n  sudo apt update\n  sudo apt install tensorplate\n'
