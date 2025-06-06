#!/usr/bin/env python3
"""
Script to set Raspberry Pi hostname to 'cedar' and configure for cedar.local mDNS

Usage:
  sudo python3 set_hostname.py /path/to/mounted/rootfs
"""

import os
import sys
import subprocess
import re

# Generated by Anthropic Claude.

def set_hostname(rootfs_path, hostname="cedar"):
    """
    Set the hostname in the mounted Raspberry Pi image
    """
    # Validate inputs
    if not os.path.isdir(rootfs_path):
        print(f"Error: {rootfs_path} is not a valid directory")
        return False

    # Set the hostname file
    hostname_path = os.path.join(rootfs_path, "etc/hostname")
    try:
        with open(hostname_path, 'w') as f:
            f.write(f"{hostname}\n")
        print(f"Successfully set hostname to '{hostname}'")
    except Exception as e:
        print(f"Failed to write hostname: {e}")
        return False

    # Update the hosts file
    hosts_path = os.path.join(rootfs_path, "etc/hosts")
    try:
        if os.path.exists(hosts_path):
            with open(hosts_path, 'r') as f:
                hosts_content = f.read()

            # Replace the existing raspberry pi hostname line or add a new one if not found
            if re.search(r'127\.0\.1\.1\s+\S+', hosts_content):
                hosts_content = re.sub(r'127\.0\.1\.1\s+\S+',
                                       f'127.0.1.1\t{hostname}', hosts_content)
            else:
                hosts_content += f"\n127.0.1.1\t{hostname}\n"

            with open(hosts_path, 'w') as f:
                f.write(hosts_content)
            print(f"Successfully updated hosts file")
        else:
            # Create a new hosts file if it doesn't exist
            with open(hosts_path, 'w') as f:
                f.write("127.0.0.1\tlocalhost\n")
                f.write(f"127.0.1.1\t{hostname}\n")
            print(f"Created new hosts file")
    except Exception as e:
        print(f"Failed to update hosts file: {e}")
        return False

    # Enable Avahi daemon (assuming it's already installed)
    try:
        # Create the systemd symlink directory if it doesn't exist
        systemd_dir = os.path.join(rootfs_path,
                                   "etc/systemd/system/multi-user.target.wants")
        os.makedirs(systemd_dir, exist_ok=True)

        # Check if Avahi service file exists
        avahi_service = os.path.join(rootfs_path,
                                     "lib/systemd/system/avahi-daemon.service")
        if os.path.exists(avahi_service):
            # Create the symlink to enable the service if it doesn't already exist
            target_link = os.path.join(systemd_dir, "avahi-daemon.service")
            if not os.path.exists(target_link):
                # Use relative symlink
                try:
                    os.symlink("../../../lib/systemd/system/avahi-daemon.service",
                               target_link)
                    print("Enabled Avahi daemon service")
                except Exception as e:
                    print(f"Warning: Failed to create symlink: {e}")
            else:
                print("Avahi daemon already enabled")
        else:
            print("Warning: Avahi daemon service file not found. Ensure avahi-daemon "
                  "is installed in the image.")
    except Exception as e:
        print(f"Failed to enable Avahi: {e}")
        return False

    return True

def main():
    ROOTFS_PATH = "/mnt/part2"
    if set_hostname(ROOTFS_PATH):
        print(f"\nSuccessfully configured hostname to 'cedar.local'")
        print(f"After first boot, the Raspberry Pi should be accessible as 'cedar.local'")
        return 0
    else:
        print(f"\nError: Failed to configure hostname")
        return 1

if __name__ == "__main__":
    sys.exit(main())
