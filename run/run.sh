#!/bin/bash

# Check for the --release flag
if [[ "$1" == "--release" ]]; then
    release_flag="--release"
    shift
fi

../src/build.sh $release_flag

# Determine the path to the built program (assumes standard Cargo structure)
if [[ -z "$release_flag" ]]; then
    binary_path="../target/debug/cedar-box-server"
else
    binary_path="../target/release/cedar-box-server"
fi

# Start the binary we just built.
"$binary_path" "$@"
