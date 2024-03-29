#!/bin/bash
# example usage: ./basic-setup.sh -c 2

export CLIENTS=2

while getopts ":c:" options; do
	case "${options}" in
		c)
			CLIENTS=${OPTARG}
			;;
		*)
			exit
			;;
	esac
done

cargo build --release
cargo build -p ya-relay-client --example http_client --release

# Start the network
RUST_LOG=debug docker compose -f test_env/docker-compose.yml up \
    --remove-orphans \
    --build client \
    --build relay_server \
    --scale client=$CLIENTS

docker compose -f test_env/docker-compose.yml down
docker image prune -f
