from python_on_whales import docker
import logging
from utils import set_netem, Cluster, Client, Server
import time
import pytest
from threading import Thread, Event

LOGGER = logging.getLogger(__name__)


class PingJob(Thread):
    client: Client
    node_id: str
    timeout: int
    before_ping: Event
    after_ping: Event

    def __init__(self, client: Client, node_id: str, timeout: int, before_ping: Event, after_ping: Event):
        super(PingJob, self).__init__()
        self.client = client
        self.node_id = node_id
        self.timeout = timeout
        self.before_ping = before_ping
        self.after_ping = after_ping

    def run(self, *args, **kwargs):
        try:
            LOGGER.info("Pinging job")
            self.before_ping.set()
            response = self.client.ping(self.node_id, timeout=self.timeout)
            assert self.node_id == response["node_id"]
            self.after_ping.set()
        except BaseException as error:
            pytest.fail(f"Ping job should not fail: {error}")


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
    assert {server.address(), client_hidden.address()} == {
        session["address"] for session in sessions_exposed["sessions"]
    }

    LOGGER.info("Disconnecting client")
    networks = cluster.disconnect(client_hidden)
    time.sleep(1)

    try:
        LOGGER.info("Pinging disconnected client")
        response = client_exposed.ping(client_hidden.node_id, timeout=3)
        pytest.fail(f"Ping should fail with timeout. Instead got: {response}")
    except BaseException as error:
        assert f"{error}".index("Read timed out") >= 0

    before_ping = Event()
    after_ping = Event()

    ping_job = PingJob(client_exposed, client_hidden.node_id, 10, before_ping, after_ping)
    ping_job.start()

    before_ping.wait(timeout=10)
    LOGGER.info("Sleepinng after ping job start.")
    time.sleep(3)

    LOGGER.info("Reconnecting client")
    cluster.connect(client_hidden, networks)

    LOGGER.info("Connected. Waiting for Ping response")
    after_ping.wait(timeout=10)

    # Consecutive pings should still work
    ping_response = client_exposed.ping(client_hidden.node_id)
    assert client_hidden.node_id == ping_response["node_id"]

    find_response = client_exposed.find(client_hidden.node_id)
    assert client_hidden.node_id in find_response["node"]["identities"]
