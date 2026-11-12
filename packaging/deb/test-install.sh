#!/usr/bin/env bash
#
# Install the given .deb files with apt, check what the cedar-box-server
# package promises, then remove them again.
#
#   test-install.sh <package-name> <file.deb>...
#
# Run as root in a throwaway bookworm arm64 environment that has the Raspberry
# Pi archive configured -- the CI builder image is exactly that. Because apt
# resolves Depends: against the same archives a Pi uses, a dependency the Pi
# cannot satisfy (a wrong libcamera soname, a too-new libc6) fails here.
#
# Checks:
#   - apt can install the packages, dependencies included
#   - postinst left the file capabilities on /usr/bin/cedar-box-server
#   - the binary starts (--help exits 0; run from a copy, see below)
#   - the cedar.service drop-in is installed
#   - removal deletes the binary and the drop-in

set -euo pipefail

package=${1:?usage: test-install.sh <package-name> <file.deb>...}
shift
[ $# -gt 0 ] || { echo "no .deb files given" >&2; exit 2; }

BIN=/usr/bin/cedar-box-server
DROPIN=/lib/systemd/system/cedar.service.d/10-cedar-server-deb.conf

fail() {
    [ -n "${GITHUB_ACTIONS:-}" ] && echo "::error::$*"
    echo "FAIL: $*" >&2
    exit 1
}

debs=()
for f in "$@"; do debs+=("$(realpath "$f")"); done

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install --yes --no-install-recommends "${debs[@]}"

echo "--- capabilities ---"
caps=$(getcap "$BIN")
echo "$caps"
for cap in cap_sys_time cap_dac_override cap_chown cap_fowner cap_net_bind_service; do
    echo "$caps" | grep --quiet "$cap" || fail "postinst did not grant $cap on $BIN"
done

# Start a capability-free copy, not $BIN itself. Docker's default bounding set
# lacks CAP_SYS_TIME, and the kernel refuses to exec a binary whose file
# capabilities (+ep) exceed the bounding set ("Operation not permitted"). A Pi
# has no such limit; what this checks is that the binary's libraries resolve.
echo "--- --help ---"
cp "$BIN" /tmp/cedar-box-server-nocaps
/tmp/cedar-box-server-nocaps --help > /dev/null \
    || fail "the packaged cedar-box-server --help exited non-zero"
rm -f /tmp/cedar-box-server-nocaps

echo "--- drop-in ---"
[ -f "$DROPIN" ] || fail "missing $DROPIN"
cat "$DROPIN"

apt-get remove --yes "$package"
[ ! -e "$BIN" ] || fail "$BIN still present after removing $package"
[ ! -e "$DROPIN" ] || fail "$DROPIN still present after removing $package"

echo "install/remove test passed for $package"
