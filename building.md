# Building and running Cedar

Cedar is a client-server system. Cedar-server runs on your Raspberry Pi and
hosts the camera, image processing algorithms, and the plate solving logic.

The client is the Cedar-aim web app that runs on your mobile phone and runs
Cedar's user interface.

## Supported platforms

These instructions are for building and running Cedar-server on a Raspberry Pi
4B (or 3B) running Bookworm. For building, at least 4GB RAM is recommended; for
running, at least 1GB RAM is recommended.

The Cedar-aim web app works with both Android and IOS devices (phones/tablets)
and also laptops (Windows/Mac/Linux). Basically, anything with a modern web
browser can run Cedar-aim.

# Using the pre-built SD card image

You can burn a pre-built image to your SD card (16GB or larger) which will boot
your Rpi directly into running Cedar. This is by far the easiest way to get
started!

## Download and burn

First: download the SD card image
[cedar_rpi_2025_feb_15.img.gz](https://storage.googleapis.com/cs-astro-files/cedar_rpi_2025_feb_15.img.gz)
to your computer.

Second: burn an SD card (32GB or larger) with the image file you just downloaded
using the [Raspberry Pi Imager](https://www.raspberrypi.com/software). Follow
these steps:

1. Choose Device: ignore this.

2. Under Choose OS, scroll to the bottom and pick Use Custom. Select the .img
   file you downloaded above.

3. Click Choose Storage and select your SD card.

4. IMPORTANT! Choose 'NO' on the 'Would you like to apply OS customization
   settings?' The SD card image already has the appropriate customizations and
   applying customizations here could break something.

5. Raspberry Pi Imager will burn and verify your SD card.

As an alternative to the above, you can use
[balenaEtcher](https://etcher.balena.io/) which bypasses the questions/answers
and just burns the SD card image.

## Using the pre-built SD card image

With the pre-built SD card you just burned, your Rpi is set up as follows:

* SSH is enabled, in case you want to poke around. Username is 'cedar', password
  is 'cedar'.
* The Rpi puts up its own Wi-Fi hot spot. The SSID is 'cedar', password is
  'cedar123'

Insert the SD card and power up your Rpi4. Wait a minute or two, then on your phone,
tablet, or laptop, join the 'cedar' Wi-Fi network (password is 'cedar123').

Now, in your device's web browser, navigate to '192.168.4.1'. You should
see Cedar's "setup" mode screen where the camera image is shown (assuming you
have a camera connected!) for focusing and aligning. Under the hamburger menu,
look for Preferences, and enable Full Screen.

See below for how to set up SkySafari to work with Cedar.

# Building from source

If you're more adventerous, you can start with a fresh Rpi OS install and build
Cedar yourself. Note that the pre-built SD card image was prepared using the same
instructions here.

## Initial steps

These instructions assume you've set up a Raspberry Pi 4 (or 3) with the Bookworm
version of Raspberry Pi OS. Make sure you've done the following:

```
sudo apt update; sudo apt full-upgrade
sudo apt install git pip protobuf-compiler libjpeg-dev zlib1g-dev libcamera-dev libclang-dev
sudo apt install python3-grpcio python3-grpc-tools
```

Before going further, if your Rpi has only 1GB of RAM, you'll need to expand its
swap space. Edit `/etc/dphys-swapfile` and change `CONF_SWAPSIZE=200` to
`CONF_SWAPSIZE=2048`. After saving the file, restart your Rpi.

### Clone repos

To build and run Cedar, you will need to clone all of the following repos, all
available at [github/smroid](https://github.com/smroid):

* asi_camera2: Rust wrapper for the ASI camera SDK.
* cedar-aim: Dart/Flutter web app. This is Cedar's user interface.
* cedar-camera: Cedar's abstraction for interfacing to cameras.
* cedar-detect: Cedar's image processing algorithms: background estimation,
  noise estimation, hot pixel repair, software binning, star detection, star
  centroiding.
* cedar-server: The server-side integration of Cedar's functionality.
* cedar-solve: Our fork of Tetra3 with significant performance and reliability
  improvements.
* tetra3_server: A gRPC encapsulation allowing Rust code to invoke Cedar-solve.

You must clone these repos into sibling directories, for example
`/home/cedar/projects/cedar-camera`, `/home/cedar/projects/cedar-detect`,
`/home/cedar/projects/cedar-server`, etc.

If `/home/cedar/projects` is your current directory, you can execute the commands:

```
git clone https://github.com/smroid/asi_camera2.git
git clone https://github.com/smroid/cedar-aim.git
git clone https://github.com/smroid/cedar-camera.git
git clone https://github.com/smroid/cedar-detect.git
git clone https://github.com/smroid/cedar-server.git
git clone https://github.com/smroid/cedar-solve.git
git clone https://github.com/smroid/tetra3_server.git
```

### Build Cedar-aim

Cedar-aim is implemented in Flutter and requires some initial setup
to get the Flutter SDK:

```
sudo apt update
sudo apt install snapd
```

At this point you need to reboot: `sudo reboot now` After rebooting, run:

```
sudo snap install snapd
sudo snap install flutter --classic
```

Run `flutter doctor` to finalize Flutter installation and verify Flutter SDK is
present. Flutter doctor will complain about a missing Android toolchain and maybe
about Chrome; these aren't needed.

Now that you have the Flutter SDK, it's time to build the Cedar-aim web app.
First:

```
dart pub global activate protoc_plugin
```

Add `/home/cedar/.pub-cache/bin` to your `PATH` environment variable.

```
./build.sh
```

### Setup Cedar-solve

Cedar-solve is implemented in Python and requires some initial setup.

In the root directory of cedar-solve (e.g. `/home/cedar/projects/cedar-solve`), run
the `setup.sh` script.

### Set up tetra3_server component

In the root directory of tetra3_server (e.g. `/home/cedar/projects/tetra3_server`), do
the following:

```
cd python
python -m grpc_tools.protoc -I../proto --python_out=. --pyi_out=. --grpc_python_out=. ../proto/tetra3.proto
```

### Enable ASI camera

If you are using an ASI camera, go to the asi_camera2 project directory and run
the `install.sh` script. You can skip this if you are using a Raspberry Pi
camera.

### Build Cedar-server

You will need to install the Rust toolchain if you don't have it already. Follow
the instructions at the [Install Rust](https://www.rust-lang.org/tools/install)
site.

Now build Cedar-server:

```
cd cedar-server
./build.sh --release
```

This builds Cedar-server and all of its dependencies. Rust crates are downloaded
and built as needed. The initial build takes around a half hour on a Rpi 4 and
well over an hour on a Rpi 3.

### Run Cedar-server

You can start the Cedar-server at the command line as follows:

```
cd cedar-server/run
source ../../cedar-solve/.cedar_venv/bin/activate
../bin/cedar-box-server
```

If things are working correctly, the output will be similar to:

```
INFO cedar_server: Using Tetra3 server "../../tetra3_server/python/tetra3_server.py" listening at "/tmp/cedar.sock"
INFO Camera camera_manager.cpp:325 libcamera v0.3.2+99-1230f78d
INFO RPI vc4.cpp:446 Registered camera /base/soc/i2c0mux/i2c@1/imx477@1a to Unicam device /dev/media4 and ISP device /dev/media1
INFO Camera camera_manager.cpp:325 libcamera v0.3.2+99-1230f78d
INFO RPI vc4.cpp:446 Registered camera /base/soc/i2c0mux/i2c@1/imx477@1a to Unicam device /dev/media4 and ISP device /dev/media1
INFO cedar_server: Using camera imx477 4056x3040
INFO cedar_server: Cedar-Box
INFO cedar_server: Copyright (c) 2024 Steven Rosenthal smr@dt3.org.
Licensed for non-commercial use.
See LICENSE.md at https://github.com/smroid/cedar-server
INFO cedar_server: Cedar server version "0.8.1" running on Raspberry Pi 4 Model B Rev 1.0/Debian GNU/Linux 12 (bookworm)
INFO cedar_server::tetra3_subprocess: Tetra3 subprocess started
WARN cedar_server::tetra3_subprocess: Loading database from: /home/cedar/projects/cedar-solve/tetra3/data/default_database.npz
WARN cedar_server: Could not read file "./cedar_ui_prefs.binpb": Os { code: 2, kind: NotFound, message: "No such file or directory" }
INFO cedar_server: Listening at 0.0.0.0:80
INFO ascom_alpaca::server: Bound Alpaca server bound_addr=[::]:11111
```

Here's what's happening:

* Cedar-server is using `/tmp/cedar.sock` to communicate with the Tetra3 server.

* The imx477 camera is detected. This is the Rpi High Quality camera.

* The `tetra3_subprocess` stars up and loads the pattern database `default_database.npz`.

* Cedar's preferences file was not found. This file will be created when the Cedar-aim
  app first saves its settings.

* Cedar-server is listening at port 80 for connections from the Cedar-aim client app.

* Cedar-server is serving the Ascom Alpaca protocol, allowing SkySafari to connect
  to the "telescope" emulated by Cedar-server.

### Run Cedar-aim

On a phone, tablet, or computer that is on the same network as the Raspberry Pi
that is running Cedar-server, use a web browser to navigate to the
IP address of your Rpi. In my case this is `cedar.local`; yours might
be something like `192.168.4.1`, depending on how your Rpi is set up on
the network.

If you're successful, you'll see the Cedar-aim setup screen. TODO: add screenshot.

### Setup SkySafari

If you have SkySafari 7 Plus or Pro, you can connect it to Cedar. To do so,
follow these steps:

1. Make sure your phone or tablet is on Cedar's wifi network.

2. With Cedar-server running, start SkySafari.

3. Menu..Settings

4. Telescope Presets

5. Add Preset

6. ASCOM Alpaca Connection

7. Choose Auto-Detect, and press Scan Network For Devices button. After a delay
   it should show CedarTelescopeEmulator in the DEVICES section. If this fails,
   try Manual Configuration with your Raspberry Pi's IP address and press the
   Check IP and Port For Devices button. If successful, it will show
   CedarTelescopeEmulator in DEVICES.

8. Next

9. Edit the Preset Name if desired.

10. Change ReadoutRate to 10 per second

11. Save Preset

12. On the main SkySafari screen, tap the Scope icon. Press Connect.

13. On the main screen, tap the `Observe` icon, choose `Scope Display`, and
    either enable `Telrad circles` or configure field of view indicators
    appropriate for your telescope setup.

Once you've succeeded in connecting SkySafari to Cedar (yay!), the SkySafari
screen will display a reticle showing your telescope's current position as
determined by Cedar's plate solving. If there is no plate solution, the
telescope position will "wiggle" as an indication that it is currently unknown.

## Next steps

Congratulations (hopefully)! You have successfully run Cedar-server, connected
to it with Cedar-aim, and (optionally) configured SkySafari to work with
Cedar-server.

There are some follow-up steps you'll need to address to be able to use Cedar
with your telescope in the field.

### Mount camera to telescope

The camera used by Cedar needs to be attached to your telescope, pointed
in the same direction as the telescope. There are two approaches, depending
on what kind of camera you have.

#### USB camera

If you are using a USB camera such as the ASI120mm mini, you can use a ring
mount to attach the camera to your scope. The Raspberry Pi running Cedar be
anywhere, with the USB cable running up to the camera, or you can also attach
the Raspberry Pi to the telescope if you prefer.

CAUTION! Be sure to use a USB2 port (black) on your Raspberry Pi, not a USB3
port (blue). USB3 is known to cause WiFi interference.

#### Raspberry Pi camera

A Raspberry Pi camera such as the HQ camera connects to the Rpi with a short and
delicate ribbon cable. You will thus need some kind of box to hold both the Rpi
and the camera, such that when the box is attached to the telescope the camera
will be pointed in the same direction as the scope.

This is an excellent job for a 3d printer. We hope to publish a suitable case
and mounting design in the cedar-serve repo in the near future.

### Setup Raspberry Pi Wi-Fi hotspot

CAUTION! If to this point you've been connected to your Rpi over Wi-Fi, please
switch to an Ethernet connection. The steps in this section will disrupt your
Rpi's connection to Wi-Fi, because this section configures your Rpi to put up
its own Wi-Fi hotspot.

The Cedar-aim client must connect over the network to Cedar-server running on
the Rpi. If you're observing from your home's rear deck you might be able to use
your home Wi-Fi, but if you're at a deep sky site your Rpi will need to provide
its own Wi-Fi hotspot.

On Bookworm, https://forums.raspberrypi.com/viewtopic.php?t=357998 has good
information on how to set up a Wi-Fi access point using NetworkManager. Here
are the steps that worked for me:

```
sudo systemctl disable dnsmasq
sudo systemctl stop dnsmasq
sudo nmcli con delete cedar-ap
sudo nmcli con add type wifi ifname wlan0 mode ap con-name cedar-ap ssid cedar autoconnect true
sudo nmcli con modify cedar-ap 802-11-wireless.band bg
sudo nmcli con modify cedar-ap 802-11-wireless.channel 9
sudo nmcli con modify cedar-ap ipv4.method shared ipv4.address 192.168.4.1/24
sudo nmcli con modify cedar-ap ipv6.method disabled
sudo nmcli con modify cedar-ap wifi-sec.key-mgmt wpa-psk
sudo nmcli con modify cedar-ap wifi-sec.psk "cedar123"
sudo nmcli con modify cedar-ap 802-11-wireless.powersave disable
sudo nmcli con up cedar-ap
```

If you want to change the ssid name or channel number, you can edit
`/etc/NetworkManager/system-connections/cedar-ap.nmconnection`. After
changing the `channel` and/or `ssid`, restart the network manger service:

```
sudo systemctl restart NetworkManager
```

### Set up service

If you want Cedar-server to start automatically when you power up your
Rpi, you can set up a systemd configuration to do this.

First, create a file `/home/cedar/run_cedar.sh` containing:

```
#!/bin/bash
source /home/cedar/projects/cedar-solve/.cedar_venv/bin/activate
cd /home/cedar/projects/cedar-server/run
export PATH=/home/cedar/.cargo/bin:$PATH
/home/cedar/projects/cedar-server/bin/cedar-box-server
```

Next, create a file `/lib/systemd/system/cedar.service` with:

```
[Unit]
Description=Cedar server.

[Service]
User=cedar
Type=simple
ExecStart=/bin/bash /home/cedar/run_cedar.sh

[Install]
WantedBy=multi-user.target
```

Use `sudo systemctl [start/stop/status/enable/disable] cedar.service` to
control the service.
