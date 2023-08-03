from python_on_whales import docker
from utils import set_netem, Cluster, Client

def test_client(compose_up):
    cluster: Cluster = compose_up(2)
    assert len(cluster.clients) == 2

    for client in cluster.clients:
        print(f"Ports: {client.ports}")
        set_netem(client.container, latency="100ms")

    client_1: Client = cluster.clients[0]
    client_2: Client = cluster.clients[1]

    ping_response = client_1.ping(client_2.node_id)
    nanos = ping_response["duration"]["nanos"]
    assert nanos > 100E6 and nanos < 100E7

    sessions_response = client_1.sessions()
    print(f"Sessions client 1: {sessions_response}")
    sessions_response = client_2.sessions()
    print(f"Sessions client 2: {sessions_response}")
