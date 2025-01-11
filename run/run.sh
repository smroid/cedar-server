#!/bin/bash

# Check for the --release flag
if [[ "$1" == "--release" ]]; then
    release_flag="--release"
    shift
fi

../src/build.sh $release_flag

# Start the binary we just built.
../bin/cedar-box-server "$@"
