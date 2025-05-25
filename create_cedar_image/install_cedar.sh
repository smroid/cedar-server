#!/bin/bash

# Copyright (c) 2025 Steven Rosenthal smr@dt3.org
# See LICENSE file in root directory for license terms.

set -e  # Exit on any error

if [ "$#" -ne 3 ]; then
    echo "Usage: $0 <path to customized_rpi.img> <path to cedar.img> <path to Cedar repos>"
    exit 1
fi

RPI_IMG_FILE="$1"
CEDAR_IMG_FILE="$2"
REPOS_PATH="$3"

echo
echo "Creating Cedar image from customized Rpi image"
cp $RPI_IMG_FILE $CEDAR_IMG_FILE

# We need stuff in this virtual env for one of our Python invocations.
source $REPOS_PATH/cedar-solve/.cedar_venv/bin/activate

echo
echo "Mounting target partitions"
sudo python mount_img.py $CEDAR_IMG_FILE

ROOT=/mnt/part2
HOMEDIR=$ROOT/home/cedar

echo
echo "Creating directories"
sudo mkdir -p $HOMEDIR/run/demo_images
sudo mkdir -p $HOMEDIR/cedar/bin
sudo mkdir -p $HOMEDIR/cedar/data
sudo mkdir -p $HOMEDIR/cedar/cedar-aim/cedar_flutter/build

echo
echo "Copying Cedar server binary"
sudo cp $REPOS_PATH/cedar-server/cedar/bin/cedar-box-server $HOMEDIR/cedar/bin/cedar-box-server

echo
echo "Copying demo images"
sudo cp $REPOS_PATH/cedar-server/run/demo_images/* $HOMEDIR/run/demo_images

echo
echo "Copying Cedar-Solve component"
sudo cp -R $REPOS_PATH/cedar-solve $HOMEDIR/cedar

echo
echo "Create virtual env"
sudo rm -rf $HOMEDIR/cedar/cedar-solve/.cedar_venv
sudo chroot $ROOT /bin/bash << 'EOF'
python -m venv /home/cedar/cedar/cedar-solve/.cedar_venv
source /home/cedar/cedar/cedar-solve/.cedar_venv/bin/activate
cd /home/cedar/cedar/cedar-solve
python -m pip install --upgrade grpcio
python -m pip install -e ".[dev,docs,cedar-detect]"
deactivate
EOF

echo
echo "Copying Tetra3 server"
sudo cp -R $REPOS_PATH/tetra3_server $HOMEDIR/cedar

echo
echo "Copying Cedar-Aim web app"
sudo cp -R $REPOS_PATH/cedar-aim/cedar_flutter/build/web \
     $HOMEDIR/cedar/cedar-aim/cedar_flutter/build

echo
echo "Copying Cedar Solve database"
sudo cp $REPOS_PATH/cedar-solve/tetra3/data/default_database.npz $HOMEDIR/cedar/data

echo
echo "Setup service to run Cedar server at startup"
sudo bash -c "cat > $ROOT/lib/systemd/system/cedar.service <<EOF
[Unit]
Description=Cedar Server
After=NetworkManager.service network-online.target cedar-ap-setup.service
Wants=NetworkManager.service network-online.target
Wants=cedar-ap-setup.service

[Service]
User=cedar
WorkingDirectory=/home/cedar/run
Type=simple
ExecStart=/bin/bash -c '. /home/cedar/cedar/cedar-solve/.cedar_venv/bin/activate && /home/cedar/cedar/bin/cedar-box-server'

[Install]
WantedBy=multi-user.target
EOF"

echo
echo "Enable services"
sudo systemctl --root=$ROOT enable cedar.service

echo
echo "Fixing file owners"
sudo chown -R 1001 $HOMEDIR/*
sudo chgrp -R 1001 $HOMEDIR/*

echo
echo "Bless Cedar server binary"
caps="cap_sys_time,cap_dac_override,cap_chown,cap_fowner,cap_net_bind_service+ep"
sudo setcap "$caps" $HOMEDIR/cedar/bin/cedar-box-server

echo
echo "Un-mounting target partitions"
sudo python mount_img.py --cleanup $CEDAR_IMG_FILE

echo
echo "Done! Use 'sudo dd if=$CEDAR_IMG_FILE of=/dev/sdc status=progress bs=4M'"
