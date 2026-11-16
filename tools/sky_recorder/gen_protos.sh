#!/bin/bash
#
# Regenerates the vendored Python gRPC bindings in sky_recorder/cedar_client
# from cedar-server's protos. Run only when the .proto files change. Requires
# grpcio-tools.
#
# Usage:  ./gen_protos.sh [PATH_TO_elements/src/proto]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROTO_DIR="${1:-$HERE/../../elements/src/proto}"
OUT="$HERE/sky_recorder/cedar_client"

if [ ! -f "$PROTO_DIR/cedar.proto" ]; then
    echo "cedar.proto not found under: $PROTO_DIR" >&2
    exit 1
fi

python -m grpc_tools.protoc \
    --experimental_allow_proto3_optional \
    --proto_path="$PROTO_DIR" \
    --python_out="$OUT" \
    --grpc_python_out="$OUT" \
    "$PROTO_DIR/cedar.proto" \
    "$PROTO_DIR/cedar_common.proto" \
    "$PROTO_DIR/cedar_sky.proto"

# protoc emits top-level `import cedar_pb2` imports; make them package-relative.
sed -i -E 's/^import (cedar[A-Za-z_]*_pb2)/from . import \1/' "$OUT"/*.py
