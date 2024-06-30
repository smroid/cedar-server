# Building and running Cedar-server

## Supported platforms

These instructions are for running Cedar-server on a Raspberry Pi 4B running
Bookworm. For building at least 4GB RAM is recommended; for running at least 1GB
RAM is recommended.

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


need to update this-- have to build cedar-aim first, so cedar-server can have
the web assets to server???



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

### Install Cedar-solve

Cedar-solve is Python-based and requires some extra setup.

In the root directory of cedar-solve (e.g. `/home/pi/projects/cedar-solve`), do
the following:

```
python -m venv .cedar_venv
source .cedar_venv/bin/activate
pip install -e ".[dev,docs,cedar-detect]"
```

You might want to add the `source .cedar_venv/bin/activate` command
to your .bashrc file.

### Set up tetra3_server component

In the root directory of tetra3_server (e.g. `/home/pi/projects/tetra3_server`), do
the following:

```
cd python
python -m grpc_tools.protoc -I../proto --python_out=. --pyi_out=. --grpc_python_out=. ../proto/tetra3.proto
```

### Build

You will need to install the Rust toolchain if you don't have it already. Follow
the instructions at the [Install Rust](https://www.rust-lang.org/tools/install)
site.

Now build Cedar-server:

```
cd cedar-server/src
./build.sh --release
```

This builds Cedar-server and all of its dependencies. Rust crates are downloaded
and built as needed. The initial build takes around 20 minutes.

### Run

You can start the Cedar-server at the command line as follows:

```
cd cedar-server/src
source ../../cedar-solve/.cedar_venv/bin/activate
../target/release/cedar-server
```

If things are working correctly, the output will be similar to:

```
INFO cedar_server: Using Tetra3 server "./tetra3_server.py" listening at "/tmp/cedar.sock"
INFO Camera camera_manager.cpp:313 libcamera v0.3.0+65-6ddd79b5
WARN RPiSdn sdn.cpp:40 Using legacy SDN tuning - please consider moving SDN inside rpi.denoise
INFO RPI vc4.cpp:446 Registered camera /base/soc/i2c0mux/i2c@1/imx477@1a to Unicam device /dev/media4 and ISP device /dev/media1
INFO Camera camera_manager.cpp:313 libcamera v0.3.0+65-6ddd79b5
WARN RPiSdn sdn.cpp:40 Using legacy SDN tuning - please consider moving SDN inside rpi.denoise
INFO RPI vc4.cpp:446 Registered camera /base/soc/i2c0mux/i2c@1/imx477@1a to Unicam device /dev/media4 and ISP device /dev/media1
INFO cedar_server: Using camera imx477 4056x3040
INFO cedar_server::tetra3_subprocess: Tetra3 subprocess started
WARN cedar_server::tetra3_subprocess: Loading database from: /home/pi/projects2/cedar-solve/tetra3/data/default_database.npz
WARN cedar_server: Could not read file "./cedar_ui_prefs.binpb": Os { code: 2, kind: NotFound, message: "No such file or directory" }
INFO cedar_server: Listening at 0.0.0.0:8080
INFO ascom_alpaca::server: Bound Alpaca server bound_addr=[::]:11111
```

Here's what's happening:

* Cedar-server is using `/tmp/cedar.sock` to communicate with the Tetra3 server.

* The imx477 camera is detected. This is the Rpi High Quality camera.

* The `tetra3_subprocess` stars up and loads the pattern database `default_database.npz`.

* Cedar's preferences file was not found. This file will be created when the Cedar-aim
  app first saves its settings.

* Cedar-server is listening at port 8080 for connections from Cedar-aim.

* Cedar-server is serving the Ascom Alpaca protocol, allowing SkySafari to connect
  to the "telescope" emulated by Cedar-server.






Cedar-aim to verify operation


## Next steps


### Raspberry Pi Wi-Fi hotspot


### Set up service
