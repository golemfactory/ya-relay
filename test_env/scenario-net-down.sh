#!/bin/bash

cargo build --release
cargo build --example http_client --release

# Start the network
docker compose -f test_env/docker-compose-2nets.yml up \
    --detach \
    --remove-orphans \
    --build client-exposed \
    --build client-hidden \
    --build relay_server \
    --scale client-exposed=1 \
    --scale client-hidden=1

#a nap
sleep 5

export EXPOSED_CONTAINER_ID=$(docker ps | grep exposed | awk '{print $1}'); echo $EXPOSED_CONTAINER_ID
export HIDDEN_CONTAINER_ID=$(docker ps | grep hidden | awk '{print $1}'); echo $HIDDEN_CONTAINER_ID

export EXPOSED_CLIENT_IP=$(docker inspect -f '{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $EXPOSED_CONTAINER_ID); echo $EXPOSED_CLIENT_IP
export HIDDEN_CLIENT_IP=$(docker inspect -f '{{range.NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $HIDDEN_CONTAINER_ID); echo $HIDDEN_CLIENT_IP

export EXPOSED_NODE_ID=$(docker logs $EXPOSED_CONTAINER_ID 2>&1 | grep "NODE ID" | awk '{print $7}' | tail -1); echo $EXPOSED_NODE_ID
export HIDDEN_NODE_ID=$(docker logs $HIDDEN_CONTAINER_ID 2>&1 | grep "NODE ID" | awk '{print $7}' | tail -1); echo $HIDDEN_NODE_ID

export NETWORK_OF_HIDDEN_CLIENT=$(docker container inspect -f '{{range $net,$v := .NetworkSettings.Networks}}{{printf "%s" $net}}{{end}}' $HIDDEN_CONTAINER_ID); echo $NETWORK_OF_HIDDEN_CLIENT

# echo "Ping hidden client from exposed client"
curl -X GET http://$EXPOSED_CLIENT_IP:8081/ping/$HIDDEN_NODE_ID

# echo "Ping exposed client from hidden client"
curl -X GET http://$HIDDEN_CLIENT_IP:8081/ping/$EXPOSED_NODE_ID

if [[ $(curl -s -X GET http://$EXPOSED_CLIENT_IP:8081/sessions | wc -l) -eq 1 ]] || [[ $(curl -s -X GET http://$HIDDEN_CLIENT_IP:8081/sessions | wc -l) -eq 1 ]];
then
  echo "Error: Initial sessions not found"
else
  echo "Initial sessions OK"
fi

curl -X GET http://$EXPOSED_CLIENT_IP:8081/sessions
curl -X GET http://$HIDDEN_CLIENT_IP:8081/sessions

# echo -e "${Blue}Disconnect hidden client from network\n"
docker network disconnect $NETWORK_OF_HIDDEN_CLIENT $HIDDEN_CONTAINER_ID

# echo -e "${Blue}Ping hidden client from exposed client\n"
curl -m 5 -X GET http://$EXPOSED_CLIENT_IP:8081/ping/$HIDDEN_NODE_ID

# echo -e "${Blue}Connect hidden client to network\n"
docker network connect $NETWORK_OF_HIDDEN_CLIENT $HIDDEN_CONTAINER_ID

# echo -e "${Blue}Ping hidden client from exposed client\n"
curl -m 5 -X GET http://$EXPOSED_CLIENT_IP:8081/find-node/$HIDDEN_NODE_ID

# sleep 5

# check if session with server exists
curl -X GET http://$EXPOSED_CLIENT_IP:8081/sessions
curl -X GET http://$HIDDEN_CLIENT_IP:8081/sessions

if [ $(curl -s -X GET http://$HIDDEN_CLIENT_IP:8081/sessions | grep 7464) ];
then
  echo "Hidden node restored session with server"
else
  echo "Error: Hidden node didn't restore session with server"
fi

# Stop the network
docker compose -f test_env/docker-compose-2nets.yml down --remove-orphans
docker image prune -f