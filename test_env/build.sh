#!/bin/bash

cd ..

docker build -t relay-server -f test_env/Dockerfile.server .
docker build -t relay-client -f test_env/Dockerfile.client .

cd test_env
