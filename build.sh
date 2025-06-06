#!/bin/bash

# Check for the --release flag
if [[ "$1" == "--release" ]]; then
    release_flag="--release"
fi

# Build with Cargo
cargo build $release_flag

# Determine the path to the built program (assumes standard Cargo structure)
if [[ -z "$release_flag" ]]; then
    binary_path="target/debug/cedar-box-server"
else
    binary_path="target/release/cedar-box-server"
fi

# Copy binary out so it survives 'cargo clean'.
mkdir -p cedar/bin
cp "$binary_path" cedar/bin

# Set capabilities.
caps="cap_sys_time,cap_dac_override,cap_chown,cap_fowner,cap_net_bind_service+ep"
sudo setcap "$caps" "$binary_path"
sudo setcap "$caps" cedar/bin/cedar-box-server
