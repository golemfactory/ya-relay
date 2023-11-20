#!/bin/bash

export CLIENT_LATENCY=3
export SERVER_LATENCY=3
export RUST_LOG=trace,mio=info,smoltcp=trace,actix_web=warn,actix_server=warn,actix_http=warn

source ./build.sh

# Start the network
docker compose -f docker-compose-nat-clients.yml up \
    --remove-orphans \
    --build client-1 \
    --build client-2 \
    --build relay_server \
    --force-recreate

docker compose -f docker-compose-nat-clients.yml down
docker image prune -f
