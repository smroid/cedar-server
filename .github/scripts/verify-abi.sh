#!/usr/bin/env bash
#
# Assert that a built cedar-box-server can actually start on the target Pi.
#
#   verify-abi.sh <binary> [max-glibc]
#
# Checks, in order:
#   1. the ELF is AArch64            -- the runner/container is really arm64
#   2. no GLIBC_ symbol above the ceiling (default 2.36, Raspberry Pi OS
#      bookworm) -- otherwise the binary dies with "GLIBC_2.xx not found"
#   3. libcamera is in DT_NEEDED     -- otherwise --features rpi-camera did not
#      link and the binary cannot drive a Pi camera
#
# This exists as a file rather than inline YAML so the same assertion can run
# from the release workflow, from Phase 2 against the binary unpacked out of
# the .deb, and by hand on a dev box or on the Pi itself.
#
# Needs only readelf (binutils), which the builder image gets via
# build-essential. Prints GitHub Actions error annotations when it detects it
# is running under Actions, plain messages otherwise.
#
# Long option names are used throughout (--wide rather than -W, and so on):
# this runs rarely and is read by people debugging a failed release, so being
# obvious beats being terse.

set -euo pipefail

BIN=${1:?usage: verify-abi.sh <binary> [max-glibc]}
MAX_GLIBC=${2:-2.36}

fail() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::error::$*"
    fi
    echo "FAIL: $*" >&2
    exit 1
}

[ -f "$BIN" ] || fail "no such file: $BIN"

# --- 1. architecture --------------------------------------------------------
echo "--- ELF header ---"
readelf --file-header "$BIN" | grep --extended-regexp 'Class|Machine|Type:'

machine=$(readelf --file-header "$BIN" | grep 'Machine:' \
            | sed --expression='s/^.*Machine:[[:space:]]*//')
if [ "$machine" != "AArch64" ]; then
    fail "$BIN is built for '$machine', not AArch64 -- the runner or the container is not arm64"
fi

# --- 2. glibc ceiling -------------------------------------------------------
# .gnu.version_r, which `readelf --version-info` prints, is the definitive
# "what do I need from libc": the versioned symbol requirements the dynamic
# linker resolves at startup.
#
# The trailing `|| true` matters: with `set -o pipefail`, a grep that matches
# nothing returns 1 and aborts the assignment, so the script would exit without
# ever reaching the diagnostic below.
echo "--- glibc versions required ---"
needs=$(readelf --wide --version-info "$BIN" \
          | grep --only-matching --extended-regexp 'GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?' \
          | sed --expression='s/^GLIBC_//' \
          | sort --unique --version-sort || true)

# An empty set is not "requires nothing" -- a dynamically linked binary always
# needs versioned libc symbols. Empty means we are looking at the wrong file,
# or readelf found no .gnu.version_r at all. Without this guard the comparison
# below silently succeeds, which is the one failure mode a check like this must
# never have.
if [ -z "$needs" ]; then
    fail "no GLIBC_ version requirements found in $BIN -- expected a dynamically linked binary; is this the right file?"
fi

echo "$needs"
worst=$(echo "$needs" | tail --lines=1)
echo "highest required: $worst (ceiling $MAX_GLIBC)"

# Version-sort the ceiling together with the highest requirement: if the
# ceiling does not come last, the binary needs something newer than the target
# has.
highest=$(printf '%s\n%s\n' "$MAX_GLIBC" "$worst" | sort --version-sort | tail --lines=1)
if [ "$highest" != "$MAX_GLIBC" ]; then
    fail "$BIN requires GLIBC_$worst but the target (bookworm) provides only $MAX_GLIBC -- it would fail to start"
fi

# --- 3. libcamera linkage ---------------------------------------------------
echo "--- shared libraries needed ---"
readelf --dynamic "$BIN" | grep NEEDED

if ! readelf --dynamic "$BIN" | grep --quiet 'libcamera\.so'; then
    fail "no libcamera in DT_NEEDED -- the rpi-camera feature did not actually link, this binary cannot drive a Pi camera"
fi

echo "ABI checks passed."
