Scripts to create a Raspberry Pi sdcard image for Cedar.

Ingredients:

Input:
<official Rpi OS .img file>
e.g. /mnt/nas/cs-astro/rpi_os_images/2024-11-19-raspios-bookworm-arm64-lite.img

customize-pi-image.sh: Creates customized_rpi_for_cedar.img from the Rpi OS .img
file, applying various customizations and modifications in preparation for
installing Cedar.

install_cedar.sh: Copies customized RPI image and installs Cedar to create
cedar.img. The various Cedar repos must have been setup and built already.
