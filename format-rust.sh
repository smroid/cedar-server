#!/bin/bash
# Debug wrapper for rustfmt
echo "Running rustfmt at $(date)" >> /tmp/rustfmt-debug.log
cd /home/pi/projects/cedar-server
rustup run nightly rustfmt --edition 2021 --config-path /home/pi/projects/cedar-server/rustfmt.toml
echo "Rustfmt exit code: $?" >> /tmp/rustfmt-debug.log
