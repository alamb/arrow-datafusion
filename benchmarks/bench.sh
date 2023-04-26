#!/bin/bash
# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

# This script is meant for developers of DataFusion -- it is runnable
# from the standard DataFusion development environment and uses cargo,
# etc.

# Exit on error
set -e

# https://stackoverflow.com/questions/59895/how-do-i-get-the-directory-where-a-bash-script-is-located-from-within-the-script
SCRIPT_DIR=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )


# Set Defaults
COMMAND=data
BENCHMARK=all
DATA_DIR=${DATA_DIR:-$SCRIPT_DIR/data}
BRANCH_NAME=$(git rev-parse --abbrev-ref HEAD)
BRANCH_NAME=${BRANCH_NAME//\//_} # mind blowing syntax to replace / with _
RESULTS_DIR=${RESULTS_DIR:-"$SCRIPT_DIR/results/$BRANCH_NAME"}
#CARGO_COMMAND=$CARGO_COMMAND:"cargo run --release"}
CARGO_COMMAND=${CARGO_COMMAND:-"cargo run --profile release-nonlto"}  # TEMP: for faster iterations

usage() {
    echo "
DataFusion Benchmark script

This script orchestrates running benchmarks for DataFusion

Usage: $0 <command> [benchmark]

**********
Examples:
**********

./bench.sh gen # Create the datasets for all benchmarks in $DATA_DIR

**********
* Commands
**********

data:         Generates data needed for benchmarking
run:          Runs the named benchmark

**********
* Benchmarks
**********

all(default): Data/Run for all benchmarks
tpch:         TPCH inspired benchmark on Scale Factor (SF) 1 (~1GB), single parquet file per table
tpch_mem:     TPCH inspired benchmark on Scale Factor (SF) 1 (~1GB), query from memory


**********
* Environment Variables
**********

The following environment variables to control this script:

DATA_DIR = directory to store datasets
CARGO_COMMAND = command that runs the benchmark binary


"
    exit 1
}

# https://stackoverflow.com/questions/192249/how-do-i-parse-command-line-arguments-in-bash
POSITIONAL_ARGS=()

while [[ $# -gt 0 ]]; do
    case $1 in
        # -e|--extension)
        #   EXTENSION="$2"
        #   shift # past argument
        #   shift # past value
        #   ;;
        -h|--help)
            shift # past argument
            usage
            ;;
        -*|--*)
            echo "Unknown option $1"
            exit 1
            ;;
        *)
            POSITIONAL_ARGS+=("$1") # save positional arg
            shift # past argument
            ;;
    esac
done

# Parse positional paraleters
set -- "${POSITIONAL_ARGS[@]}" # restore positional parameters
COMMAND=${1:-"${COMMAND}"}
BENCHMARK=${2:-"${BENCHMARK}"}


# Do what is requested
main() {
    echo "***************************"
    echo "DataFusion Benchmark Script"
    echo "COMMAND: ${COMMAND}"
    echo "BENCHMARK: ${BENCHMARK}"
    echo "BRACH_NAME: ${BRANCH_NAME}"
    echo "DATA_DIR: ${DATA_DIR}"
    echo "RESULTS_DIR: ${RESULTS_DIR}"
    echo "CARGO_COMMAND: ${CARGO_COMMAND}"
    echo "***************************"

    # Command Dispatch
    case "$COMMAND" in
        data)
            case "$BENCHMARK" in
                all)
                    data_tpch
                    ;;
                tpch)
                    data_tpch
                    ;;
                tpch_mem)
                    # same data for tpch_mem
                    data_tpch
                    ;;
                *)
                    echo "Error: unknown benchmark '$BENCHMARK' for data gen"
                    usage
                    ;;
            esac
            ;;
        run)
            mkdir -p "${RESULTS_DIR}"
            case "$BENCHMARK" in
                all)
                    run_tpch
                    run_tpch_mem
                    ;;
                tpch)
                    run_tpch
                    ;;
                tpch_mem)
                    ;;
                *)
                    echo "Error: unknown benchmark '$BENCHMARK' for run"
                    usage
                    ;;
            esac
            ;;
        *)
            echo "Error: unknown command: $COMMAND"
            usage
            ;;
    esac
}



# Creates TPCH data if it doesn't already exist
data_tpch() {
    echo "Creating tpch dataset..."

    # Ensure the target data directory exists
    mkdir -p "${DATA_DIR}"

    # Create 'tbl' (CSV format) data into $DATA_DIR if it does not already exist
    SCALE_FACTOR=1
    FILE="${DATA_DIR}/supplier.tbl"
    if test -f "${FILE}"; then
        echo " tbl files exist ($FILE exists)."
    else
        echo " creating tbl files with tpch_dbgen..."
        docker run -v "${DATA_DIR}":/data -it --rm ghcr.io/databloom-ai/tpch-docker:main -vf -s ${SCALE_FACTOR}
    fi

    # Copy expected answers into the ./data/answers directory if it does not already exist
    FILE="${DATA_DIR}/answers/q1.out"
    if test -f "${FILE}"; then
        echo " Expected answers exist (${FILE} exists)."
    else
        echo " Copying answers to ${DATA_DIR}/answers"
        mkdir -p "${DATA_DIR}/answers"
        docker run -v "${DATA_DIR}":/data -it --entrypoint /bin/bash --rm ghcr.io/databloom-ai/tpch-docker:main -c "cp -f /opt/tpch/2.18.0_rc2/dbgen/answers/* /data/answers/"
    fi

    # Create 'parquet' files from tbl
    FILE="${DATA_DIR}/supplier"
    if test -d "${FILE}"; then
        echo " parquet files exist ($FILE exists)."
    else
        echo " creating parquet files using benchmark binary ..."
        $CARGO_COMMAND --bin tpch -- convert --input "${DATA_DIR}" --output "${DATA_DIR}" --format parquet
    fi
}

# Runs the tpch benchmark
run_tpch() {
    RESULTS_FILE="${RESULTS_DIR}/tpch.json"
    echo "RESULTS_FILE: ${RESULTS_FILE}"
    echo "Running tpch benchmark..."
    $CARGO_COMMAND --bin tpch -- benchmark datafusion --iterations 5 --path "${DATA_DIR}" --format parquet -o ${RESULTS_FILE}
}

# Runs the tpch in memory
run_tpch_mem() {
    RESULTS_FILE="${RESULTS_DIR}/tpch_mem.json"
    echo "RESULTS_FILE: ${RESULTS_FILE}"
    echo "Running tpch_mem benchmark..."
    # -m means in memory
    #TEMP only use query 1
    $CARGO_COMMAND --bin tpch -- benchmark datafusion  --iterations 5 --path "${DATA_DIR}" -m --format parquet -o ${RESULTS_FILE}
}


# And start the process up
main
