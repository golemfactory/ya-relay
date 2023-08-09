from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import time


def test_net_down(compose_up):
    cluster: Cluster = compose_up(2, build_args={"RUST_LOG": "trace"})
    # cluster: Cluster = compose_up(2)

    server: Server = cluster.servers()[0]
    client_exposed: Client = cluster.clients()[0]
    client_hidden: Client = cluster.clients()[1]

    find_response = client_exposed.find(client_hidden.node_id)
    assert client_hidden.node_id in find_response["node"]["identities"]

    sessions_exposed = client_exposed.sessions()
    assert {server.address()} == {session["address"] for session in sessions_exposed["sessions"]}

    ping_response = client_exposed.ping(client_hidden.node_id)
    assert client_hidden.node_id == ping_response["node_id"]

    sessions_exposed = client_exposed.sessions()
    print(f"Client exposed sessions {sessions_exposed}")
    assert {server.address(), client_hidden.address()} == {
        session["address"] for session in sessions_exposed["sessions"]
    }

    networks = cluster.disconnect(client_hidden)
    print(f"Disconnected {client_hidden.node_id} from {networks}")

    time.sleep(5)

    try:
        response = client_exposed.ping(client_hidden.node_id, timeout=70)
        print(f"Ping response: {response}")
    except BaseException as error:
        print(f"Ping error: {error}")
