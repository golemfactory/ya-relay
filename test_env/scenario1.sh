#!/bin/bash

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

export EXPOSED_NODE_ID=$(docker logs $EXPOSED_CONTAINER_ID 2>&1 | grep "NODE ID" | awk '{print $6}'); echo $EXPOSED_NODE_ID
export HIDDEN_NODE_ID=$(docker logs $HIDDEN_CONTAINER_ID 2>&1 | grep "NODE ID" | awk '{print $6}'); echo $HIDDEN_NODE_ID

set -vx

# Find clients
# echo "Find hidden client from exposed client"
curl -X GET http://$EXPOSED_CLIENT_IP:8081/find-node/$HIDDEN_NODE_ID

# echo "Find exposed client from hidden client"
curl -X GET http://$HIDDEN_CLIENT_IP:8081/find-node/$EXPOSED_NODE_ID

# echo "Ping hidden client from exposed client"
curl -X GET http://$EXPOSED_CLIENT_IP:8081/ping/$HIDDEN_NODE_ID

# echo "Ping exposed client from hidden client"
curl -X GET http://$HIDDEN_CLIENT_IP:8081/ping/$EXPOSED_NODE_ID

# echo -e "${Blue}Disconnect hidden client from network\n"
export NETWORK_OF_HIDDEN_CLIENT=$(docker container inspect -f '{{range $net,$v := .NetworkSettings.Networks}}{{printf "%s" $net}}{{end}}' $HIDDEN_CONTAINER_ID)
docker network disconnect $NETWORK_OF_HIDDEN_CLIENT $HIDDEN_CONTAINER_ID

# echo -e "${Blue}Ping hidden client from exposed client\n"
curl --connect-timeout 5 -X GET http://$EXPOSED_CLIENT_IP:8081/ping/$HIDDEN_NODE_ID


# echo "Find hidden client from exposed client"
curl --connect-timeout 5 -X GET http://$EXPOSED_CLIENT_IP:8081/find-node/$HIDDEN_NODE_ID

# echo "Find exposed client from hidden client"
curl --connect-timeout 5 -X GET http://$HIDDEN_CLIENT_IP:8081/find-node/$EXPOSED_NODE_ID

# echo -e "${Blue}Connect hidden client to network\n"
docker network connect $NETWORK_OF_HIDDEN_CLIENT $HIDDEN_CONTAINER_ID

# echo -e "${Blue}Ping hidden client from exposed client\n"
curl -X GET http://$EXPOSED_CLIENT_IP:8081/ping/$HIDDEN_NODE_ID


# echo "Find hidden client from exposed client"
curl -X GET http://$EXPOSED_CLIENT_IP:8081/find-node/$HIDDEN_NODE_ID

# echo "Find exposed client from hidden client"
curl -X GET http://$HIDDEN_CLIENT_IP:8081/find-node/$EXPOSED_NODE_ID

# Stop the network
docker compose -f test_env/docker-compose-2nets.yml down --remove-orphans