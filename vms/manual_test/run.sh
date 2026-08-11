#!/bin/sh
#-device virtio-blk-pci,id=vd0,drive=drive0,num-queues=4 \
#-drive file=focal-server-cloudimg-amd64.img,format=raw,if=none,id=drive0 \
#-kernel /home/bijan/Documents/research/linux-mm-econ/kbuild/arch/x86_64/boot/bzImage \
#-kernel /home/bijan/Documents/research/linux-file-only-mem/kbuild/arch/x86_64/boot/bzImage \
#-append "console=ttyS0,115200n8 nokaslr memmap=3G!4G memmap=3G!7G" \

SCRIPT_DIR=$(dirname $(readlink -f $0))
if [ "$1" = "v" ]; then
    echo "Running in volatile mode"
    SNAPSHOT=""
else
    SNAPSHOT="-snapshot"
fi

$HOME/qemu/build/qemu-system-x86_64 \
-m 4G,slots=4,maxmem=8G \
-smp 1,sockets=1,cores=1,threads=1 \
-object memory-backend-memfd,size=4G,id=m0,reserve=off,share=off,hugetlb=on,hugetlbsize=2M,donatable=on \
-numa node,cpus=0,memdev=m0,nodeid=0 \
-machine type=q35,cxl=on,hmat=on \
-cpu host,mce=on,lmce=on \
-append "console=ttyS0,115200n8 kgdboc=ttyS1,115200 transparent_hugepage=never nokaslr" \
-kernel $HOME/guest-kernel/kbuild/arch/x86_64/boot/bzImage \
-initrd $SCRIPT_DIR/../initramfs.cpio \
-nographic \
-serial mon:stdio \
-serial pty \
-device virtio-blk-pci,id=vd0,drive=drive0,num-queues=4 \
-drive file=$HOME/imgs/ubuntu_vm.qcow2,format=qcow2,if=none,id=drive0 \
-device virtio-blk-pci,id=vd1,drive=drive1,num-queues=4 \
-drive file=$HOME/imgs/cloud_init.iso,format=raw,if=none,id=drive1 \
-device virtio-net-pci,netdev=net0 \
-netdev user,id=net0,hostfwd=tcp::5555-:22,hostfwd=tcp::27017-:27017 \
-device pxb-cxl,id=cxl.0,bus=pcie.0,bus_nr=52 \
-device cxl-rp,id=rp0,bus=cxl.0,chassis=0,port=0,slot=0 \
-object memory-backend-ram,id=mem0,size=4G \
-device cxl-type3,bus=rp0,num-dc-regions=1,volatile-dc-memdev=mem0,id=cxl-mem0,sn=6666 \
-M cxl-fmw.0.targets.0=cxl.0,cxl-fmw.0.size=4G,cxl-fmw.0.use-ram=true \
-qmp unix:/tmp/qmp-sock,server,nowait \
-d int,cpu_reset \
-D /tmp/qemu_log \
-enable-kvm \
$SNAPSHOT
