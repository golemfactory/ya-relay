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

source ./build.sh

# Start the network
docker compose -f docker-compose.yml up \
    --remove-orphans \
    --build client \
    --build relay_server \
    --scale client=$CLIENTS

docker compose -f docker-compose.yml down
docker image prune -f
