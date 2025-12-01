#!/usr/bin/env bash
set -e

# CONFIG
ROOT="$(pwd)"
SPARK_VERSION="v3.5.1"
SPARK_SRC="$ROOT/spark"
TPCDS_KIT="$ROOT/tpcds-kit"
TPCDS_DATA="$ROOT/tpcds-data/scale1"
CORES=$(nproc)

echo "Using $CORES CPU cores"

# 1. Install dependencies
echo "==> Installing build dependencies..."
sudo apt-get update
sudo apt-get install -y \
    git wget curl openjdk-17-jdk \
    maven scala python3 python3-pip \
    gcc g++ make automake autoconf libtool \
    flex bison byacc

# 2. Clone & Build Spark
echo "==> Cloning Apache Spark..."
if [ ! -d "$SPARK_SRC" ]; then
    git clone https://github.com/apache/spark.git "$SPARK_SRC"
fi

cd "$SPARK_SRC"
git fetch --all
git checkout "$SPARK_VERSION"

echo "==> Building Spark (full build, Hive + ThriftServer)..."
./build/mvn -DskipTests -T ${CORES}C \
    -Pyarn -Phive -Phive-thriftserver \
    clean package

echo "==> Spark build completed."

cd ..

# Download TPC-DS performance test
git clone git@github.com:yslys/spark-tpc-ds-performance-test.git
echo "TPC-DS performance test cloned."
cd spark-tpc-ds-performance-test
echo "Modify ./bin/tpcdsenv.sh to set SPARK_HOME to /home/user/spark on VM, /mydata/spark on local machine."
echo "Run ./bin/tpcdsspark.sh to generate tables and run queries."

# 3. Download & Build TPC-DS toolkit
# echo "==> Downloading TPC-DS toolkit..."
# cd "$ROOT"
# if [ ! -d "$TPCDS_KIT" ]; then
#     git clone https://github.com/databricks/tpcds-kit.git "$TPCDS_KIT"
# fi

# echo "==> Building TPC-DS..."
# cd "$TPCDS_KIT/tools"
# make OS=LINUX -j$CORES

# echo "==> TPC-DS toolkit built."

# 4. Generate 1GB TPC-DS dataset
# echo "==> Generating 1GB TPC-DS dataset..."
# mkdir -p "$TPCDS_DATA"

# cd "$TPCDS_KIT/tools"
# ./dsdgen \
#     -SCALE 1 \
#     -DIR "$TPCDS_DATA" \
#     -FORCE Y

# echo "==> 1GB dataset generated at: $TPCDS_DATA"

# 5. Copy tpcds.sql schema
# echo "==> Copying tpcds.sql schema..."
# if [ -f "$TPCDS_KIT/tools/tpcds.sql" ]; then
#     cp "$TPCDS_KIT/tools/tpcds.sql" "$TPCDS_DATA/"
# else
#     echo "ERROR: tpcds.sql not found!"
#     exit 1
# fi

# echo "==> Schema copied to: $TPCDS_DATA/tpcds.sql"

#############################################
# DONE
#############################################
echo "=================================================="
echo "  Spark BUILD COMPLETED"
echo "  SPARK HOME: $SPARK_SRC"
# echo "  TPCDS KIT : $TPCDS_KIT"
# echo "  DATASET   : $TPCDS_DATA  (1GB)"
echo "=================================================="

