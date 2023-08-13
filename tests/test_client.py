from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server


# Always use test_ prefix. Otherwise `pytest` will not notice test method.
# `compose_up` is an fixture defined in `conftest.py`.
def test_client(compose_up):
    # Factory fixture produces function `compose_up`
    cluster: Cluster = compose_up(2)

    server: Server = cluster.servers()[0]
    client_1: Client = cluster.clients()[0]
    client_2: Client = cluster.clients()[1]

    sessions_1 = client_1.sessions()
    print(f"Sessions client 1: {sessions_1}")
    # DICT["DICT_KEY"]["LIST_INDEX"]["NESTED_DICT_KEY"]
    assert sessions_1["sessions"][0]["address"] == server.address()

    client_1.ping(client_2.node_id)

    sessions_1 = client_1.sessions()
    print(f"Sessions client 1: {sessions_1}")
    # {FUNCTION(ELEMENT) for ELEMENT in ITERABLE} produces Set
    # [FUNCTION(ELEMENT) for ELEMENT in ITERABLE] produces List
    assert {server.address(), client_2.address()} == {session["address"] for session in sessions_1["sessions"]}

    sessions_2 = client_2.sessions()
    print(f"Sessions client 2: {sessions_2}")
    assert {server.address(), client_1.address()} == {session["address"] for session in sessions_2["sessions"]}

    find_response = client_2.find(client_1.node_id)
    print(f"Find client 1: {find_response}")
    assert client_1.node_id in find_response["node"]["identities"]

    data = bytearray(1_050_000)
    transfer_response = client_1.transfer(client_2.node_id, data, timeout=10)
    print(f"Transfer client 2: {transfer_response}")
    assert 1 == transfer_response["mb_transfered"]
    assert client_2.node_id == transfer_response["node_id"]

    # defined in `shared/utils.py`
    set_netem(client_2, latency="100ms")

    ping_response = client_1.ping(client_2.node_id)
    print(f"Ping client 2: {ping_response}")
    assert ping_response["duration"] in range(100, 200)
