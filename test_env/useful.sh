
# disconnect docker container 'test_env-client-2' from network 'test_env_client-network-2'
docker network disconnect test_env_client-network-2 $(docker ps --filter "name=test_env-client-2" --format='{{.ID}}')

# ping node at port 1999 from node at port 1998
curl localhost:1998/ping/reliable/$(curl localhost:1999/info 2&>1 | jq -r '.node_id')