#!/usr/bin/env python3
import subprocess
import os
import re
import sys
import time
from contextlib import contextmanager

class DeviceManager:
    def __init__(self, image_path):
        self.image_path = image_path
        self.loop_devices = []

    def setup_loop_devices(self):
        """Run kpartx and capture the loop device names"""
        try:
            result = subprocess.run(['kpartx', '-v', '-a', self.image_path],
                                 capture_output=True, text=True, check=True)

            # Parse kpartx output to get loop devices
            # Example output line: "add map loop5p1 (253:0): 0 524288 linear 7:5 8192"
            loop_matches = re.finditer(r'add map (loop\dp\d)', result.stdout)
            self.loop_devices = [match.group(1) for match in loop_matches]

            if len(self.loop_devices) < 2:
                raise RuntimeError(
                    f"Expected at least 2 partitions, found {len(self.loop_devices)}")

            print(f"Created loop devices: {', '.join(self.loop_devices)}")
            return True

        except subprocess.CalledProcessError as e:
            print(f"Error running kpartx: {e}")
            print(f"kpartx stderr: {e.stderr}")
            return False

    def cleanup(self):
        """Remove loop devices"""
        try:
            subprocess.run(['kpartx', '-d', self.image_path], check=True)
            print("Removed loop devices")
        except subprocess.CalledProcessError as e:
            print(f"Error removing loop devices: {e}")

    def get_root_partition(self):
        """Get the root partition device (typically the second one)"""
        if len(self.loop_devices) < 2:
            raise RuntimeError("No root partition found")
        # Return the second partition (index 1)
        return f"/dev/mapper/{self.loop_devices[1]}"

def resize_filesystem(image_path):
    """Resize the filesystem on the root partition"""
    manager = DeviceManager(image_path)

    try:
        # Setup loop devices
        if not manager.setup_loop_devices():
            return False

        # Get root partition
        root_dev = manager.get_root_partition()
        print(f"Root partition device: {root_dev}")

        # Wait a moment for devices to settle
        time.sleep(1)

        # Run filesystem check
        print("Running filesystem check...")
        try:
            subprocess.run(['e2fsck', '-f', root_dev], check=False)
        except subprocess.CalledProcessError as e:
            # e2fsck returns non-zero for various states, don't treat as error
            print(f"e2fsck returned: {e.returncode}")

        # Resize filesystem
        print("Resizing filesystem...")
        result = subprocess.run(['resize2fs', root_dev],
                                capture_output=True, text=True)
        print(result.stdout)
        if result.stderr:
            print(result.stderr)

        print("Filesystem operations completed successfully")
        return True

    except Exception as e:
        print(f"Error during filesystem operations: {e}")
        return False

    finally:
        # Always try to cleanup
        manager.cleanup()

def main():
    if len(sys.argv) != 2:
        print("Usage: sudo python3 resize_fs.py <image_file>")
        sys.exit(1)

    image_path = sys.argv[1]

    if not os.path.exists(image_path):
        print(f"Image file {image_path} not found")
        sys.exit(1)

    success = resize_filesystem(image_path)
    sys.exit(0 if success else 1)

if __name__ == "__main__":
    main()
