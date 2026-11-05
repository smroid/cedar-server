#!/bin/bash
# Debug wrapper for rustfmt
echo "Running rustfmt at $(date)" >> /tmp/rustfmt-debug.log
cd /home/pi/projects/cedar-server
# Same nightly as RUSTFMT_TOOLCHAIN in .github/workflows/ci.yml, so formatting
# here matches the CI check. Install it once with:
#   rustup toolchain install nightly-2026-10-20 --profile minimal -c rustfmt
rustup run nightly-2026-10-20 rustfmt --edition 2021 --config-path /home/pi/projects/cedar-server/rustfmt.toml
echo "Rustfmt exit code: $?" >> /tmp/rustfmt-debug.log
