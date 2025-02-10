#!/bin/bash

# Check for the --release flag
if [[ "$1" == "--release" ]]; then
    release_flag="--release"
    shift
fi

./build.sh $release_flag

. ../cedar-solve/.cedar_venv/bin/activate
cd run

# Start the binary we just built.
../bin/cedar-box-server "$@"
