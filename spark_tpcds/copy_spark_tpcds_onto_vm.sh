#!/usr/bin/env bash
set -euo pipefail

sudo apt-get update
sudo apt-get install -y libguestfs-tools

#  Clean local Spark metadata
clean_metadata() {
    local TARGET_DIR=$1
    echo "Checking Spark metadata under: $TARGET_DIR"

    # Find and delete metastore_db
    find "$TARGET_DIR" -type d -name "metastore_db" -print -exec rm -rf {} + 2>/dev/null

    # Find and delete spark-warehouse
    find "$TARGET_DIR" -type d -name "spark-warehouse" -print -exec rm -rf {} + 2>/dev/null

    # Delete Derby lock files
    find "$TARGET_DIR" -type f -name "db.lck" -print -exec rm -f {} + 2>/dev/null
    find "$TARGET_DIR" -type f -name "dbex.lck" -print -exec rm -f {} + 2>/dev/null
    find "$TARGET_DIR" -type f -name "service.properties" -print -exec rm -f {} + 2>/dev/null

    echo "Metadata cleanup complete for: $TARGET_DIR"
    echo ""
}

echo "=== Cleaning local Spark folders before copying ==="
clean_metadata "./spark"
clean_metadata "./spark-tpc-ds-performance-test"

#  Mount VM disk image

cd vm_guide

mkdir -p tmp_rootfs
sudo guestmount -a noble-server-cloudimg-amd64.qcow2 -m /dev/sda1 --rw ./tmp_rootfs

cd ..

#  Copy cleaned Spark into VM
echo "Copying Spark onto VM ..."
sudo cp -r ./spark ./vm_guide/tmp_rootfs/home/user/
echo "Spark copied onto VM"

echo "Copying Spark TPC-DS Performance Test onto VM ..."
sudo cp -r ./spark-tpc-ds-performance-test ./vm_guide/tmp_rootfs/home/user/
echo "Spark TPC-DS Performance Test copied onto VM"

# Change ownership of the home directory to user (no longer root)
sudo chown -R 1000:1000 ./vm_guide/tmp_rootfs/home/user

#  Unmount VM disk
sudo guestunmount ./vm_guide/tmp_rootfs
echo "VM unmounted"
echo "Done"
