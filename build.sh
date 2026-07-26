#!/bin/bash

# Check for the --release and --asi flags (any order). Default is the Raspberry
# Pi camera; pass --asi for a unit using a ZWO ASI camera instead (requires
# asi_camera2/install.sh to have been run first).
release_flag=""
camera_feature="rpi-camera"
for arg in "$@"; do
    case "$arg" in
        --release)
            release_flag="--release"
            ;;
        --asi)
            camera_feature="asi-camera"
            ;;
        *)
            echo "Unknown argument: $arg"
            exit 1
            ;;
    esac
done

# Build with Cargo
# Statically link libjpeg-turbo for SIMD-accelerated JPEG encoding.
TURBOJPEG_STATIC=1 cargo build $release_flag --features "$camera_feature"

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
