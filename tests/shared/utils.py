from python_on_whales import docker, DockerClient, Container
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Dict, List, TypeVar, Set
import json
import re
import requests
import threading
import sys
import logging

http_client_headers = {"Accept": "application/json"}

LOGGER = logging.getLogger(__name__)


def read_node_id(container: Container) -> str:
    until = datetime.now() + timedelta(seconds=60)
    node_id_log = b"CLIENT NODE ID: "
    node_id = ""
    for _, log in docker.logs(container, until=until, stream=True, follow=True):  # type: ignore
        if node_id_log in log:
            node_id = log.split(node_id_log)[1].strip().decode("utf-8")
            break
    assert re.match(r"0x[0-9a-fA-F]{40}", node_id), f"Invalid client Node Id {node_id} of container: {vars(container)}"

    return node_id


def read_json_response(response: requests.Response):
    # Ok if 20x. Exception otherwise.
    if response.status_code in range(200, 299):
        j = json.loads(response.content)
        LOGGER.debug(f"Response: {j}")
        return j
    else:
        LOGGER.info(f"Fail: {response}")
        raise Exception(response.status_code)


class Node:
    container: Container

    def __init__(self, container: Container):
        self.container = container

    def ports(self) -> Dict[str, int]:
        ports = self.container.network_settings.ports
        if ports is None:
            return {}
        return {
            key: int(next(item["HostPort"] for item in value if item["HostIp"] == "0.0.0.0"))
            for key, value in ports.items()
        }

    def address(self, name_part: str = "public") -> str | None:
        LOGGER.debug(f"Looking for {name_part}")
        LOGGER.debug(f"Net settings: {self.container.network_settings}")
        networks = self.container.network_settings.networks
        if networks is None:
            return None
        if name_part == None and len(networks) == 1:
            return list(networks.values())[0].ip_address
        ips = []
        for name in networks:
            if name_part in name:
                ips.append(networks[name].ip_address)
        if len(ips) == 0:
            return None
        if len(ips) > 1:
            raise Exception(
                "Test error. Multiple ips [{ips}] for network name part: {name_part}",
            )
        return ips[0]


class Server(Node):
    def __init__(self, container: Container):
        Node.__init__(self, container=container)


TClient = TypeVar("TClient", bound="Client")


class Client(Node):
    node_id: str

    def __init__(self, container: Container):
        Node.__init__(self, container=container)
        self.node_id = read_node_id(container=container)

    def __external_port(self, port: int = 8081):
        return self.ports()[f"{port}/tcp"]

    def ping(self, node_id: str, port: int = 8081, timeout: int | None = 5, transport: str = "reliable"):
        LOGGER.debug(f"GET Ping node {node_id} ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.get(
            f"http://localhost:{port}/ping/{transport}/{node_id}", headers=http_client_headers, timeout=timeout
        )
        response = read_json_response(response)
        return response

    def sessions(self, port: int = 8081, timeout: int | None = 5):
        LOGGER.debug(f"GET Sessions ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.get(
            f"http://localhost:{port}/sessions", headers=http_client_headers, timeout=timeout
        )
        return read_json_response(response)

    def find(self, node_id: str, port: int = 8081, timeout: int | None = 5):
        LOGGER.debug(f"GET Find node {node_id} ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.get(
            f"http://localhost:{port}/find-node/{node_id}", headers=http_client_headers, timeout=timeout
        )
        return read_json_response(response)

    def transfer(
        self, node_id: str, data: bytes, port: int = 8081, timeout: int | None = None, transport: str = "reliable"
    ):
        LOGGER.debug(f"POST Transfer file to {node_id} ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.post(
            f"http://localhost:{port}/transfer-file/{transport}/{node_id}",
            data,
            headers=http_client_headers,
            timeout=timeout,
        )
        return read_json_response(response)


class LoggerJob(threading.Thread):
    compose_client: DockerClient

    def __init__(self, compose_client: DockerClient):
        super(LoggerJob, self).__init__()
        self.compose_client = compose_client

    def run(self, *args, **kwargs):
        for _, line in self.compose_client.compose.logs([], follow=True, stream=True, timestamps=False):
            line = line.decode("utf-8")
            line = line.strip()
            LOGGER.debug(line)
        LOGGER.info(f"Logger stopped")


@dataclass
class Scales:
    relay_server: int
    public_client: int
    alice_client: int
    bob_client: int


class Cluster:
    docker_client: DockerClient
    logger_job: LoggerJob

    def __init__(self, compose_client: DockerClient):
        self.docker_client = compose_client
        self.logger_job = LoggerJob(compose_client)

    def start(self, scales: Scales | Dict):
        LOGGER.info(f"Docker Compose Up ({scales})")
        self.docker_client.compose.up(detach=True, wait=True, scales=vars(scales))
        self.logger_job.start()
        if isinstance(scales, Scales):
            assert len(self.clients()) == scales.public_client + scales.alice_client + scales.bob_client
            assert len(self.servers()) == scales.relay_server
        LOGGER.info(f"Successfully started")

    def __find_container(self, name_part: str) -> List[Container]:
        containers = []
        for container in self.docker_client.container.list():
            # LOGGER.info(f"[find-container]: Container: {container.name}")
            if name_part in container.name:
                containers.append(container)
        return containers

    def clients(self, name_part: str = "_client") -> List[Client]:
        containers = self.__find_container(name_part)
        return [Client(container) for container in containers]

    def servers(self, name_part: str = "relay_server") -> List[Server]:
        containers = self.__find_container(name_part)
        return [Server(container) for container in containers]

    def disconnect(self, node: Node) -> Set[str]:
        networks = set()
        if isinstance(node.container.network_settings.networks, Dict):
            for network in node.container.network_settings.networks:
                LOGGER.info(f"Disconnecting {node} from {network}")
                self.docker_client.network.disconnect(network, node.container)
                networks.add(network)
        return networks

    def connect(self, node: Node, networks: Set[str]):
        for network in networks:
            LOGGER.info(f"Connecting {node} to {network}")
            self.docker_client.network.connect(network, node.container)


def set_netem(node: Node, latency="0ms"):
    LOGGER.info(f"Set netem for '{node.container.name}', latency {latency}")
    docker.execute(
        container=node.container,
        command=["tc", "qdisc", "replace", "dev", "eth0", "root", "netem", "delay", latency],
        privileged=True,
    )
