#!/usr/bin/env bash
set -euo pipefail

# Inside VM, install dependencies for Spark and TPC-DS performance test.
sudo apt-get update && sudo apt-get upgrade -y
sudo apt-get install -y \
    git wget curl  openjdk-17-jdk\
    maven scala python3 python3-pip \
    gcc g++ make automake autoconf libtool \
    flex bison byacc