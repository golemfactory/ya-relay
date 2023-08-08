from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import time


def test_net_down(compose_up):
    cluster: Cluster = compose_up(2, build_args={"RUST_LOG": "debug"})
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

    time.sleep(5)
    # time.sleep(60)

    resp = client_exposed.ping(client_hidden.node_id, timeout=10)

    # with pytest.raises(Exception) as err:
    #     resp = client_exposed.ping(client_hidden.node_id)
    #     print(f"Ping disconnected {client_hidden.node_id}: {resp}")
    # assert 404 == err

    # assert client_hidden.node_id in find_response["node"]["identities"]

    # sessions_response = client_1.sessions()
    # print(f"Sessions client 1: {sessions_response}")
    # sessions_response = client_2.sessions()
    # print(f"Sessions client 2: {sessions_response}")
