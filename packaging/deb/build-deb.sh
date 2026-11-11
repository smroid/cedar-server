#!/usr/bin/env bash
#
# Package a built cedar-box-server as a Debian .deb for a Cedar Raspberry Pi.
#
#   build-deb.sh --binary <path> --version <debian-version> --out <dir>
#                [--package <name>] [--depends <deps>] [--provides <name>]
#                [--dropin-extra <file>] [--summary <text>]
#
# The package contains the server binary and nothing else that the Cedar image
# already provides -- no Cedar Aim web app, no Python tetra3 environment:
#
#   /usr/bin/cedar-box-server
#   /lib/systemd/system/cedar.service.d/10-cedar-server-deb.conf
#   /usr/share/doc/<package>/copyright
#
# The drop-in points the image's existing cedar.service at /usr/bin; postinst
# restores the file capabilities a .deb cannot carry, then restarts the service
# if it was running. See cedar.service.conf and postinst next to this script.
#
# Options:
#   --package <name>      package name (default: cedar-server)
#   --depends <deps>      extra Depends, appended to the computed shared-library
#                         dependencies, e.g. "foo (>= 1.0), bar"
#   --provides <name>     declare this a drop-in replacement for <name>: sets
#                         Provides, Conflicts and Replaces, so the two packages
#                         cannot be installed together
#   --dropin-extra <file> lines appended to the [Service] drop-in, e.g.
#                         Environment= settings for a variant build
#   --summary <text>      one-line Description (has a sensible default)
#
# Run it inside the bookworm builder image (or on a bookworm arm64 box): it
# uses dpkg-shlibdeps, which reads the shared-library dependency info of the
# installed libcamera/libc packages, so Depends: names exactly the packages and
# minimum versions the binary was linked against. Run elsewhere, the computed
# dependencies would describe the wrong distribution.
#
# Maintainer is $DEB_MAINTAINER if set, otherwise derived from the GitHub repo
# owner. Homepage is the GitHub repo the build ran in.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$here/../.." && pwd)

binary="" version="" out="" package="cedar-server" extra_depends=""
provides="" dropin_extra=""
summary="Cedar plate-solving electronic finder server for Raspberry Pi"

while [ $# -gt 0 ]; do
    case "$1" in
        --binary)       binary=$2; shift 2 ;;
        --version)      version=$2; shift 2 ;;
        --out)          out=$2; shift 2 ;;
        --package)      package=$2; shift 2 ;;
        --depends)      extra_depends=$2; shift 2 ;;
        --provides)     provides=$2; shift 2 ;;
        --dropin-extra) dropin_extra=$2; shift 2 ;;
        --summary)      summary=$2; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$binary" ] && [ -n "$version" ] && [ -n "$out" ] || {
    echo "usage: $0 --binary <path> --version <version> --out <dir> [options]" >&2
    exit 2
}
[ -f "$binary" ] || { echo "no such binary: $binary" >&2; exit 1; }
if [ -n "$dropin_extra" ] && [ ! -f "$dropin_extra" ]; then
    echo "no such file: $dropin_extra" >&2; exit 1
fi

owner=${GITHUB_REPOSITORY_OWNER:-cedar}
maintainer=${DEB_MAINTAINER:-"$owner <$owner@users.noreply.github.com>"}
homepage="https://github.com/${GITHUB_REPOSITORY:-smroid/cedar-server}"

work=$(mktemp --directory)
trap 'rm -rf "$work"' EXIT
stage="$work/stage"

# --- Files ----------------------------------------------------------------
install -D --mode=0755 "$binary" "$stage/usr/bin/cedar-box-server"

dropin="$stage/lib/systemd/system/cedar.service.d/10-cedar-server-deb.conf"
install -D --mode=0644 "$here/cedar.service.conf" "$dropin"
if [ -n "$dropin_extra" ]; then
    { echo; cat "$dropin_extra"; } >> "$dropin"
fi

install -D --mode=0644 "$repo_root/LICENSE.md" \
    "$stage/usr/share/doc/$package/copyright"

install -D --mode=0755 "$here/postinst" "$stage/DEBIAN/postinst"
install -D --mode=0755 "$here/postrm"   "$stage/DEBIAN/postrm"

# --- Dependencies ---------------------------------------------------------
# dpkg-shlibdeps insists on a debian/control in the current directory, even
# though it only needs the package's name from it.
mkdir --parents "$work/src/debian"
printf 'Source: %s\n\nPackage: %s\nArchitecture: arm64\n' \
    "$package" "$package" > "$work/src/debian/control"
shlibs=$(cd "$work/src" \
         && dpkg-shlibdeps -O "-e$stage/usr/bin/cedar-box-server" 2>/dev/null \
         | sed --quiet 's/^shlibs:Depends=//p')
[ -n "$shlibs" ] || {
    echo "dpkg-shlibdeps produced no dependencies; is this a bookworm arm64 environment?" >&2
    (cd "$work/src" && dpkg-shlibdeps -O "-e$stage/usr/bin/cedar-box-server") >&2 || true
    exit 1
}
# libcap2-bin provides setcap, which postinst needs.
depends="$shlibs, libcap2-bin"
[ -n "$extra_depends" ] && depends="$depends, $extra_depends"

# --- Control file ---------------------------------------------------------
installed_size=$(du --summarize --block-size=1K "$stage" | cut --fields=1)
{
    echo "Package: $package"
    echo "Version: $version"
    echo "Architecture: arm64"
    echo "Maintainer: $maintainer"
    echo "Installed-Size: $installed_size"
    echo "Depends: $depends"
    if [ -n "$provides" ]; then
        echo "Provides: $provides"
        echo "Conflicts: $provides"
        echo "Replaces: $provides"
    fi
    echo "Section: science"
    echo "Priority: optional"
    echo "Homepage: $homepage"
    echo "Description: $summary"
    echo " Installs cedar-box-server to /usr/bin and points the Cedar image's"
    echo " existing cedar.service at it through a systemd drop-in. The file"
    echo " capabilities the server needs (binding port 80, setting the clock)"
    echo " are applied at install time."
    echo " ."
    echo " Built for Raspberry Pi OS bookworm (glibc 2.36, libcamera 0.5) with"
    echo " Raspberry Pi camera support. It replaces only the server binary: the"
    echo " Cedar Aim web app and the Python tetra3 solver still come from the"
    echo " Cedar image. Removing the package reverts to the image's binary."
} > "$stage/DEBIAN/control"

# --- Build ----------------------------------------------------------------
mkdir --parents "$out"
deb="$out/${package}_${version}_arm64.deb"
dpkg-deb --root-owner-group -Zxz --build "$stage" "$deb" >/dev/null

echo "built $deb"
dpkg-deb --info "$deb"
dpkg-deb --contents "$deb"
