#!/bin/bash

# Copyright (c) 2025 Steven Rosenthal smr@dt3.org
# See LICENSE file in root directory for license terms.

set -e  # Exit on any error

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <raspberrypi-os.img> <customized_rpi.img>"
    exit 1
fi

SOURCE_IMG_FILE="$1"
CUSTOMIZED_RPI_FILE="$2"

if [ ! -f "$SOURCE_IMG_FILE" ]; then
    echo "Error: File $SOURCE_IMG_FILE does not exist"
    exit 1
fi

# This script sets up the Pi OS for Cedar, but does not install any
# of the Cedar components.

echo "Setting up Raspberry Pi OS image for use with Cedar"

echo "Copying to $CUSTOMIZED_RPI_FILE"
cp $SOURCE_IMG_FILE $CUSTOMIZED_RPI_FILE

echo "Extending $CUSTOMIZED_RPI_FILE"
dd if=/dev/zero bs=1M count=3500 >> $CUSTOMIZED_RPI_FILE
echo "Resizing filesystem"
sudo parted $CUSTOMIZED_RPI_FILE resizepart 2 100%
sudo python resize_fs.py $CUSTOMIZED_RPI_FILE

echo
echo "Mounting target partitions"
sudo python mount_img.py $CUSTOMIZED_RPI_FILE
BOOT_PATH="/mnt/part1"
ROOTFS_PATH="/mnt/part2"

echo
echo "Update/upgrade OS"
sudo python update_upgrade_chroot.py

echo
echo "Update boot cmdline.txt"
sudo python modify_cmdline.py

echo
echo "Expand swap"
sudo python modify_swap.py

echo
echo "Enable i2c"
sudo sed -i 's/#dtparam=i2c_arm=on/dtparam=i2c_arm=on/g' $BOOT_PATH/config.txt
echo "i2c-dev" | sudo tee -a $ROOTFS_PATH/etc/modules

echo
echo "Add user 'pi'"
sudo python create_userconf.py

echo
echo "Add user 'cedar'"
sudo python add_rpi_user.py

echo
echo "Set hostname to 'cedar'"
sudo python set_hostname.py

echo
echo "Enable ssh"
sudo python enable_ssh.py

echo
echo "Setup WiFi access point on first boot"
sudo python install-cedar-ap-setup.py

echo
echo "Un-mounting target partitions"
sudo python mount_img.py --cleanup $CUSTOMIZED_RPI_FILE

echo
echo "Done! Next step is install_cedar.sh"
