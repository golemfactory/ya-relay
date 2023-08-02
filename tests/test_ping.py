from python_on_whales import docker
from utils import set_netem, Cluster

def test_ping(compose_up):
    cluster: Cluster = compose_up(2)
    for client in cluster.clients:
        set_netem(client.container, latency="100ms")
