This directory contains the spark workload and some scripts that automate the VM-build process.

Note: all scripts are supposed to be run in `/mydata`.

+ `build_vm_linux.sh`: Compile Linux, create disk image and run in QEMU
+ `build_spark_tpc-ds.sh`: Compile Spark locally, clone the TPC-DS 1GB dataset
+ `copy_spark_tpcds_onto_vm.sh`: Copy spark and TPC-DS onto VM
+ `inside_vm.sh`: Install dependencies inside VM
