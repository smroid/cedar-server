# Building and running Cedar-server

## Supported platforms

These instructions are for running Cedar-server on a Raspberry Pi 4B running
Bookworm. For building Cedar-server at least 4GB RAM is recommended; for running
Cedar-server at least 1GB RAM is recommended.

## Initial steps

### Clone repos

To build and run Cedar-server, you will need to clone all of the following repos,
all available at [github/smroid](https://github.com/smroid):

* asi_camera2
* cedar-camera
* cedar-detect
* cedar-server
* cedar-solve
* tetra3_server

Note the client app is [Cedar-aim](https://github.com/smroid/cedar-aim); it has
its own instructions on how to build and run. The remainder of this document
concerns Cedar-server only.

You must clone these repos into sibling directories, for example
`/home/pi/projects/cedar-camera`, `/home/pi/projects/cedar-detect`,
`/home/pi/projects/cedar-server`, etc.

If `/home/pi/projects` is your current directory, you can execute
the commands:

```
git clone https://github.com/smroid/asi_camera2.git
git clone https://github.com/smroid/cedar-camera.git
git clone https://github.com/smroid/cedar-detect.git
git clone https://github.com/smroid/cedar-server.git
git clone https://github.com/smroid/cedar-solve.git
git clone https://github.com/smroid/tetra3_server.git
```

### Build

You will need to install the Rust toolchain if you don't have it already. Follow
the instructions at the [Install Rust](https://www.rust-lang.org/tools/install)
site.

Build Cedar-server:

```
cd cedar-server/src
./build.sh --release
```

This builds Cedar-server and all of its dependencies. Rust crates are downloaded
and built as needed.

### Run



Cedar-aim to verify operation


## Next steps


### Raspberry Pi Wi-Fi hotspot


### Set up service
