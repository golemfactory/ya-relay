from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server

def test_client(compose_up):
    cluster: Cluster = compose_up(clients=2, file="docker-compose-nat-clients.yml")

    server: Server = cluster.servers()[0]
    client_1: Client = cluster.clients()[0]
    client_2: Client = cluster.clients()[1]

    client_1.ping(client_2.node_id)
