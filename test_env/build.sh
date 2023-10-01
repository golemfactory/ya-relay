#!/bin/bash

cargo build --release
cargo build -p ya-relay-client --example http_client --release
for d in debug release
do
  mkdir -p target/$d/examples
  cp ../target/$d/ya-relay-server target/$d/ 2>/dev/null || :
  cp ../target/$d/examples/http_client target/$d/examples/ 2>/dev/null || :
done

docker build -f Dockerfile.base -t tests_base .
