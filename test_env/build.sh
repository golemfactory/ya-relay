#!/bin/bash

cargo build --release
cargo build -p ya-relay-client --example http_client --release

docker build -f Dockerfile.base -t tests_base .
