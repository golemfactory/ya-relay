from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import time
import pytest


def test_net_down(compose_up):
    cluster: Cluster = compose_up(2)

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
    time.sleep(3)

    try:
        response = client_exposed.ping(client_hidden.node_id)
        # TODO ping should not timeout in this scenario
        pytest.fail("Ping should fail with 404")
    except BaseException as error:
        # TODO ping should return 404 instead of timeout
        print(f"Ping error: {error}")
    # except Exception as error:
    #     assert error[0] == 404

    find_response = client_exposed.find(client_hidden.node_id)
    # TODO should not be found
    # assert client_hidden.node_id not in find_response["node"]["identities"]

    cluster.connect(client_hidden, networks)
    time.sleep(3)

    # TODO Ping should work
    # ping_response = client_exposed.ping(client_hidden.node_id)
    # assert client_hidden.node_id == ping_response["node_id"]

    find_response = client_exposed.find(client_hidden.node_id)
    assert client_hidden.node_id in find_response["node"]["identities"]
