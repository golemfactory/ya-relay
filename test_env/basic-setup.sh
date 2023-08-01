#!/bin/bash

cargo build --release
cargo build --example http_client --release

# Start the network
docker compose -f test_env/docker-compose.yml up \
    --remove-orphans \
    --build client \
    --build relay_server \
    --scale client=2
