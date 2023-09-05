from python_on_whales import docker
import logging
from utils import set_netem, Cluster, Client, Server
import time
import pytest
from threading import Thread, Event
import math

LOGGER = logging.getLogger(__name__)


class TransferJob(Thread):
    client: Client
    node_id: str
    data: bytes
    timeout: int
    before_tr: Event
    after_tr: Event

    def __init__(self, client: Client, node_id: str, data: bytes, timeout: int, before_tr: Event, after_tr: Event):
        super(TransferJob, self).__init__()
        self.client = client
        self.node_id = node_id
        self.data = data
        self.timeout = timeout
        self.before_tr = before_tr
        self.after_tr = after_tr

    def run(self, *args, **kwargs):
        try:
            LOGGER.info("Transfer job")
            self.before_tr.set()
            response = self.client.transfer(self.node_id, self.data, timeout=self.timeout)
            LOGGER.info(f"Transfer response: {response}")
            assert self.node_id == response["node_id"]
            expected_len = math.floor(len(self.data) / 1_000_000)
            assert expected_len == response["mb_transfered"]
            self.after_tr.set()
        except BaseException as error:
            pytest.fail(f"Transfer job should not fail: {error}")


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


class Jobs(Thread):
    jobs: [Thread]
    jobs_started: Event

    def __init__(self, ping_jobs: [Thread], ping_jobs_started: Event):
        super(Jobs, self).__init__()
        self.jobs = ping_jobs
        self.jobs_started = ping_jobs_started

    def run(self, *args, **kwargs):
        for job in self.jobs:
            job.start()
        self.jobs_started.set()


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

    before_tr_events = []
    after_tr_events = []
    tr_jobs = []
    for i in range(0, 3): 
        before_tr = Event()
        after_tr = Event()
        data = bytearray(1_050_000)
        tr_job = TransferJob(client_exposed, client_hidden.node_id, data, 10, before_tr, after_tr)
        tr_jobs.append(tr_job)
        before_tr_events.append(before_tr)
        after_tr_events.append(after_tr)
    tr_jobs_started = Event()
    tr_jobs = Jobs(tr_jobs, tr_jobs_started)

    before_ping_events = []
    after_ping_events = []
    ping_jobs = []
    for i in range(0, 30):
        before_ping = Event()
        after_ping = Event()
        ping_job = PingJob(client_exposed, client_hidden.node_id, 10, before_ping, after_ping)
        ping_jobs.append(ping_job)
        before_ping_events.append(before_ping)
        after_ping_events.append(after_ping)
    ping_jobs_started = Event()
    ping_jobs = Jobs(ping_jobs, ping_jobs_started)

    LOGGER.info("Starting ping jobs and transfer job.")
    ping_jobs.start()
    tr_jobs.start()

    for before_tr in before_tr_events:
        before_tr.wait(timeout=10)  # It might last n*10s. Ignore it for now.
    for before_ping in before_ping_events:
        before_ping.wait(timeout=10)

    LOGGER.info("Sleepinng after jobs start.")
    time.sleep(3)

    LOGGER.info("Reconnecting client")
    cluster.connect(client_hidden, networks)

    LOGGER.info("Connected. Waiting for Ping and Transfer jobs to finnish")
    for after_tr in after_tr_events:
        after_tr.wait(timeout=10)
    for after_ping in after_ping_events:
        after_ping.wait(timeout=10)

    # Consecutive pings should still work
    ping_response = client_exposed.ping(client_hidden.node_id)
    assert client_hidden.node_id == ping_response["node_id"]

    find_response = client_exposed.find(client_hidden.node_id)
    assert client_hidden.node_id in find_response["node"]["identities"]
