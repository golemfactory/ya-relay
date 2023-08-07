from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import pytest
import time

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
    assert {server.address(), client_hidden.address()} == {session["address"] for session in sessions_exposed["sessions"]}

    print(f"Client e {client_exposed}")
    print(f"Client h {client_hidden}")
    
    print(f"Net settings {client_hidden.container.network_settings}")
    for network in client_hidden.container.network_settings.networks:
        print(f"Disconnect {client_hidden.node_id} from {network}")
        cluster.docker_client.network.disconnect(network, client_hidden.container)

    time.sleep(5)

    try:
        print(f"Try to ping")
        resp = client_exposed.ping(client_hidden.node_id)
        print(f"Ping disconnected {client_hidden.node_id}: {resp}")
    except BaseException as exception:
        print(f"Fail {exception}")


    # with pytest.raises(Exception) as err:
    #     resp = client_exposed.ping(client_hidden.node_id)
    #     print(f"Ping disconnected {client_hidden.node_id}: {resp}")
    # assert 404 == err

    # assert client_hidden.node_id in find_response["node"]["identities"]

    # sessions_response = client_1.sessions()
    # print(f"Sessions client 1: {sessions_response}")
    # sessions_response = client_2.sessions()
    # print(f"Sessions client 2: {sessions_response}")
