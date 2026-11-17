#!/usr/bin/env bash
#
# Print the Debian package version for the current checkout.
#
#   version.sh [tag]
#
# With a tag (vX.Y.Z or vX.Y.Z-suffix): the tag minus its leading "v". The tag
# must start with the version in server/Cargo.toml, so a release cannot be
# labelled with a version the binary does not report.
#
# Without a tag, a snapshot version that sorts after the Cargo version it was
# built from and after any earlier snapshot:
#
#   <cargo version>+git<committer date, UTC YYYYMMDDHHMMSS>.<short sha>
#   e.g. 1.7.5+git20261108050716.77349c2
#
# The committer date (not the author date) is used so that a rebased branch
# gets a newer version even when its commits are old.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$here/../.." && pwd)

cargo_version=$(sed --quiet 's/^version = "\(.*\)"$/\1/p' "$repo_root/server/Cargo.toml" | head --lines=1)
[ -n "$cargo_version" ] || { echo "no version in server/Cargo.toml" >&2; exit 1; }

tag=${1:-}
if [ -n "$tag" ]; then
    version=${tag#v}
    case "$version" in
        "$cargo_version"|"$cargo_version"-*) ;;
        *)
            echo "tag $tag does not match server/Cargo.toml version $cargo_version" >&2
            exit 1 ;;
    esac
    echo "$version"
else
    stamp=$(TZ=UTC git -C "$repo_root" log -1 --format=%cd --date=format-local:%Y%m%d%H%M%S)
    sha=$(git -C "$repo_root" rev-parse --short=7 HEAD)
    echo "${cargo_version}+git${stamp}.${sha}"
fi
