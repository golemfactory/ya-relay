#!/bin/bash

source ./build.sh

# Start the network
docker compose -f docker-compose-2nets.yml up \
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

export EXPOSED_NODE_ID=$(docker logs $EXPOSED_CONTAINER_ID 2>&1 | grep "NODE ID" | sed -E 's/^.*(0x[a-f0-9]{40}).*$/\1/g' | tail -1); echo $EXPOSED_NODE_ID
export HIDDEN_NODE_ID=$(docker logs $HIDDEN_CONTAINER_ID 2>&1 | grep "NODE ID" | sed -E 's/^.*(0x[a-f0-9]{40}).*$/\1/g' | tail -1); echo $HIDDEN_NODE_ID

export NETWORK_OF_HIDDEN_CLIENT=$(docker container inspect -f '{{range $net,$v := .NetworkSettings.Networks}}{{printf "%s" $net}}{{end}}' $HIDDEN_CONTAINER_ID); echo $NETWORK_OF_HIDDEN_CLIENT

# Ping hidden client from exposed client
curl -w "\n" -X GET http://$EXPOSED_CLIENT_IP:8081/ping/unreliable/$HIDDEN_NODE_ID

# Ping exposed client from hidden client
curl -w "\n" -X GET http://$HIDDEN_CLIENT_IP:8081/ping/unreliable/$EXPOSED_NODE_ID

if [[ $(curl -s -X GET http://$EXPOSED_CLIENT_IP:8081/sessions | wc -l) -eq 1 ]] || [[ $(curl -s -X GET http://$HIDDEN_CLIENT_IP:8081/sessions | wc -l) -eq 1 ]];
then
  echo "Error: Initial sessions not found"
else
  echo "Initial sessions OK"
fi

echo "exposed client sessions"
curl -w "\n" -X GET http://$EXPOSED_CLIENT_IP:8081/sessions
echo "hidden client sessions"
curl -w "\n" -X GET http://$HIDDEN_CLIENT_IP:8081/sessions

# Disconnect hidden client from network
docker network disconnect $NETWORK_OF_HIDDEN_CLIENT $HIDDEN_CONTAINER_ID

# Give hidden node some time to close session with server
sleep 30

# Ping hidden client from exposed client
curl -w "\n" -m 5 -X GET http://$EXPOSED_CLIENT_IP:8081/ping/unreliable/$HIDDEN_NODE_ID

# Connect hidden client to network
docker network connect $NETWORK_OF_HIDDEN_CLIENT $HIDDEN_CONTAINER_ID

# Ping hidden client from exposed client
curl -w "\n" -m 5 -X GET http://$HIDDEN_CLIENT_IP:8081/ping/unreliable/$EXPOSED_NODE_ID

# Give hidden node some time to restore session with server
sleep 5

# check if session with server exists
echo "exposed client sessions"
curl -w "\n" -X GET http://$EXPOSED_CLIENT_IP:8081/sessions
echo "hidden client sessions"
curl -w "\n" -X GET http://$HIDDEN_CLIENT_IP:8081/sessions

if [ $(curl -s -X GET http://$HIDDEN_CLIENT_IP:8081/sessions | grep 7464) ];
then
  echo "Hidden node restored session with server"
else
  echo "Error: Hidden node didn't restore session with server"
fi

# Stop the network
docker compose -f docker-compose-2nets.yml down --remove-orphans
docker image prune -f
