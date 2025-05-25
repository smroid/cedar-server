from pathlib import Path
import shutil

def install_ap_setup(root_mount):
    # Create directories if needed
    sbin_path = Path(root_mount) / 'usr/local/sbin'
    systemd_path = Path(root_mount) / 'etc/systemd/system'
    sbin_path.mkdir(parents=True, exist_ok=True)
    systemd_path.mkdir(parents=True, exist_ok=True)

    # Copy the files
    shutil.copy2('cedar-ap-setup.py', sbin_path / 'cedar-ap-setup.py')
    shutil.copy2('cedar-ap-setup.service', systemd_path / 'cedar-ap-setup.service')
    shutil.copy2('cedar-ap-power.service', systemd_path / 'cedar-ap-power.service')

    # Ensure script is executable
    script_path = sbin_path / 'cedar-ap-setup.py'
    script_path.chmod(0o755)

    # After copying the files, enable the services
    enable_path = Path(root_mount) / 'etc/systemd/system/multi-user.target.wants'
    enable_path.mkdir(parents=True, exist_ok=True)
    enable_link1 = enable_path / 'cedar-ap-setup.service'
    enable_link1.symlink_to('../cedar-ap-setup.service')
    enable_link2 = enable_path / 'cedar-ap-power.service'
    enable_link2.symlink_to('../cedar-ap-power.service')

def main():
    ROOTFS_PATH = "/mnt/part2"

    try:
        install_ap_setup(ROOTFS_PATH)
        print(f"Successfully installed AP setup")
    except Exception as e:
        print(f"Error installing AP setup: {e}")

if __name__ == "__main__":
    main()
