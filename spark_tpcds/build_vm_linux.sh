#!/usr/bin/env bash
set -euo pipefail

################################################################################
# It will build the Linux kernel, busybox, and create an initramfs.
# It will also create a cloud-init config file and a cloud-init seed image.
# It will extend the Ubuntu cloud disk image to +64GB for Spark.
################################################################################

KVER="6.12.4"                   # Your Linux version
KERNEL_SRC="linux-$KVER"       # Folder name after extraction
BUSYBOX_VER="1.37.0"
JOBS=$(nproc)

sudo apt-get update && sudo apt-get upgrade -y
sudo apt-get install autoconf automake autotools-dev curl libmpc-dev libmpfr-dev libgmp-dev \
                 gawk build-essential bison flex texinfo gperf libtool patchutils bc \
                 zlib1g-dev libexpat-dev git qemu-system

# 1. PREPARE WORK DIR
mkdir -p vm_guide
cd vm_guide

# 2. DOWNLOAD + EXTRACT LINUX KERNEL
if [ ! -d "$KERNEL_SRC" ]; then
    echo "Downloading Linux kernel $KVER ..."
    curl -LO https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KVER.tar.xz
    tar -xf linux-$KVER.tar.xz
fi

# 3. BUILD LINUX KERNEL (BARE METAL BUILD)
cd "$KERNEL_SRC"

echo "Configuring Linux kernel ..."
make defconfig

# Apply required config options
./scripts/config --enable CONFIG_BLK_DEV_INITRD
./scripts/config --enable CONFIG_PCI
./scripts/config --enable CONFIG_BINFMT_ELF
./scripts/config --enable CONFIG_SERIAL_8250
./scripts/config --enable CONFIG_NET
./scripts/config --enable CONFIG_PACKET
./scripts/config --enable CONFIG_UNIX
./scripts/config --enable CONFIG_INET
./scripts/config --disable CONFIG_WIRELESS
./scripts/config --enable CONFIG_ATA
./scripts/config --enable CONFIG_NETDEVICES
./scripts/config --enable CONFIG_NET_VENDOR_REALTEK
./scripts/config --enable CONFIG_8139TOO
./scripts/config --disable CONFIG_WLAN
./scripts/config --enable CONFIG_DEVTMPFS
./scripts/config --enable CONFIG_VIRTIO
./scripts/config --enable CONFIG_VIRTIO_BLK
./scripts/config --enable CONFIG_VIRTIO_NET
./scripts/config --enable CONFIG_ISO9660_FS
./scripts/config --enable CONFIG_EXT4_FS

echo "Building Linux kernel ..."
make -j"$JOBS"

KERNEL_IMAGE=$(realpath arch/x86/boot/bzImage)
echo "Kernel built: $KERNEL_IMAGE"

cd ..

# 4. CREATE INITRAMFS ROOT DIR
echo "Setting up initramfs root ..."
rm -rf initramfs
mkdir -pv initramfs/{bin,lib,lib64,dev,etc,mnt/root,proc,root,sbin,sys}

# 5. DOWNLOAD + BUILD BUSYBOX
if [ ! -d "busybox-$BUSYBOX_VER" ]; then
    echo "Downloading BusyBox ..."
    curl -LO https://busybox.net/downloads/busybox-$BUSYBOX_VER.tar.bz2
    tar -xf busybox-$BUSYBOX_VER.tar.bz2
fi

cd busybox-$BUSYBOX_VER
make defconfig
make -j"$JOBS"
make install

# Copy busybox installation into initramfs
cp -avR _install/* ../initramfs/
cd ..

# 6. COPY SHARED LIBRARIES (FOLLOW LDD)
echo "Copying shared libraries ..."
cd initramfs

# Show dependencies
ldd bin/busybox

mkdir -pv lib/x86_64-linux-gnu lib64

# libc, libm, libresolv
cp -aLv /lib/x86_64-linux-gnu/libm.so.6 lib/x86_64-linux-gnu/
cp -aLv /lib/x86_64-linux-gnu/libc.so.6 lib/x86_64-linux-gnu/
cp -aLv /lib/x86_64-linux-gnu/libresolv* lib/x86_64-linux-gnu/

# ld-linux loader
cp -aLv /lib64/ld-linux-x86-64.so.2 lib64/

# 7. CREATE INIT SCRIPT
cat << 'EOF' > init
#!/bin/sh

mount -t proc none /proc
mount -t sysfs none /sys

# Minimal device nodes
mknod -m 666 /dev/ttyS0 c 4 64
mknod -m 622 /dev/console c 5 1
mknod -m 666 /dev/null c 1 3
mknod -m 666 /dev/tty c 5 0

mdev -s

echo -e "\nBoot took $(cut -d' ' -f1 /proc/uptime) seconds\n"

exec /bin/sh
EOF

chmod +x init

# 8. PACKAGE INTO INITRAMFS
echo "Creating initramfs ..."
find . -print0 | cpio --null -ov --format=newc > ../my-initramfs.cpio

cd .. # go back to vm_guide directory

# 9. Add user to kvm group
kvm-ok || echo "KVM not available"
sudo usermod -a -G kvm $USER

echo "Initramfs created: $(realpath my-initramfs.cpio)"
echo ""
echo "============================================="
echo " BUILD COMPLETE"
echo " Kernel:    $KERNEL_IMAGE"
echo " Initramfs: $(realpath my-initramfs.cpio)"
echo "============================================="


echo "Verify Running QEMU ..."
: << 'EOF'
/usr/bin/qemu-system-x86_64 \
    	-m 1024 \
    	-smp 4,sockets=1,cores=4,threads=1 \
    	-kernel ./linux-6.12.4/arch/x86/boot/bzImage \
    	-initrd ./my-initramfs.cpio \
    	-append "console=ttyS0,115200n8" \
    	-nographic \
	-enable-kvm \
	-cpu host
EOF

# 10. Download Ubuntu Cloud Image
wget -O noble-server-cloudimg-amd64.qcow2 \
https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img
qemu-img resize noble-server-cloudimg-amd64.qcow2 +64G

sudo apt-get install cloud-utils # to install cloud-* utils

# 11. Create cloud-init config files
echo "Creating cloud-init config (cloud.yaml, cloud-net.yaml)..."
echo "Please check your public key path, the default is ~/.ssh/id_rsa.pub"
echo "This script will exit, once you make sure the public key is correct, please run the script again."
echo "Don't forget to remove the exit 0 line in the script."
# exit 0

cat <<EOF > cloud.yaml
#cloud-config
ssh_pwauth: False
users:
  - name: user
    plain_text_passwd: password
    lock_passwd: False
    ssh_authorized_keys:
      - $(cat ~/.ssh/id_rsa.pub)
    sudo: ['ALL=(ALL)NOPASSWD:ALL']
    groups: sudo
    shell: /bin/bash
EOF

cat <<EOF > cloud-net.yaml
version: 2
ethernets:
  interface0:
    match:
      name: enp*
    dhcp4: true
EOF

# 12. Generate cloud-init seed image (cloud.img)
echo "Generating cloud-init seed image cloud.img ..."
cloud-localds -v --network-config=cloud-net.yaml cloud.img cloud.yaml

# 13. Replace init script to support switch_root
echo "Replacing init script for switch_root boot into Ubuntu Cloud Image ..."

rm -f initramfs/init

cat << 'EOF' > initramfs/init
#!/bin/sh

mount -t proc none /proc
mount -t sysfs none /sys
mknod -m 666 /dev/ttyS0 c 4 64
mknod -m 622 /dev/console c 5 1
mknod -m 666 /dev/null c 1 3
mknod -m 666 /dev/tty c 5 0
mdev -s

echo -e "\nBoot took $(cut -d' ' -f1 /proc/uptime) seconds\n"

echo "Switching root..."

mkdir /newroot
mount /dev/vda1 /newroot

mount --move /sys /newroot/sys
mount --move /proc /newroot/proc
mount --move /dev /newroot/dev

echo -e "\nWelcome to Ubuntu Cloud.\n$(uname -a)\n"

exec switch_root /newroot /sbin/init
EOF

chmod +x initramfs/init

# 14. Repack initramfs after modifying init
echo "Repacking initramfs ..."
cd initramfs
find . -print0 | cpio --null -ov --format=newc > ../my-initramfs.cpio
cd ..
echo "Updated initramfs created!"

# 15. Final QEMU command template
cat << 'EOF'
=============================================
 All Done! Use this QEMU command to boot VM: (Inside vm_guide)
=============================================

sudo /usr/bin/qemu-system-x86_64 \
    -m 8192 \
    -smp 4,sockets=1,cores=4,threads=1 \
    -kernel ./linux-6.12.4/arch/x86/boot/bzImage \
    -initrd ./my-initramfs.cpio \
    -append "console=ttyS0,115200n8" \
    -nographic \
    -device virtio-blk-pci,id=vd0,drive=drive0,num-queues=4 \
    -drive file=./noble-server-cloudimg-amd64.qcow2,format=qcow2,if=none,id=drive0 \
    -device virtio-blk-pci,id=vd1,drive=drive1,num-queues=4 \
    -drive file=./cloud.img,format=raw,if=none,id=drive1 \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::5555-:22 \
    -enable-kvm \
    -cpu host

login: user
password: password
EOF

echo "============================================="
echo " Build + cloud-init integration complete!"
echo "============================================="

