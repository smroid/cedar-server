#!/bin/bash

# Check for the --release flag
if [[ "$1" == "--release" ]]; then
    release_flag="--release"
fi

# Build with Cargo
cargo build $release_flag

# Determine the path to the built program (assumes standard Cargo structure)
if [[ -z "$release_flag" ]]; then
    binary_path="../target/debug/cedar-server"
else
    binary_path="../target/release/cedar-server"
fi

# Set capabilities
sudo setcap cap_sys_time+ep "$binary_path"

# Start the binary we just built.
$binary_path
