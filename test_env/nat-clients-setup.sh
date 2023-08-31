#!/bin/bash
# example usage: ./basic-setup.sh -c 2

export CLIENT_LATENCY=3
export SERVER_LATENCY=3
export RUST_LOG=trace,mio=info,smoltcp=trace,actix_web=warn,actix_server=warn,actix_http=warn

cargo build --release
cargo build --example http_client --release

# Start the network
docker compose -f test_env/docker-compose-nat-clients.yml up \
    --remove-orphans \
    --build client-1 \
    --build client-2 \
    --build relay_server

docker compose -f test_env/docker-compose.yml down
docker image prune -f
