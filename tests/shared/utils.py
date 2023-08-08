from python_on_whales import docker, DockerClient, Container
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Dict, List, TypeVar, Set
import json
import re
import requests
import threading
import sys

http_client_headers = {"Accept": "application/json"}


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
    if response.status_code == 200:
        j = json.loads(response.content)
        print(f"Response: {j}")
        return j
    else:
        print(f"Fail: {response}")
        raise Exception((response.status_code, response.content))


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

    def address(self, net_name_part="relay-network") -> str | None:
        # print(f"Looking for {net_name_part}")
        # print(f"Net settings: {self.container.network_settings}")
        networks = self.container.network_settings.networks
        if networks is None:
            return None
        for name in networks:
            if net_name_part in name:
                return networks[name].ip_address
        return None


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

    def ping(self, node_id: str, port: int = 8081, timeout: int = 5):
        print(f"GET Ping node {node_id} ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.get(
            f"http://localhost:{port}/ping/{node_id}", headers=http_client_headers, timeout=timeout
        )
        return read_json_response(response)

    def sessions(self, port: int = 8081, timeout: int = 5):
        print(f"GET Sessions ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.get(
            f"http://localhost:{port}/sessions", headers=http_client_headers, timeout=timeout
        )
        return read_json_response(response)

    def find(self, node_id: str, port: int = 8081, timeout: int = 5):
        print(f"GET Find node {node_id} ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.get(
            f"http://localhost:{port}/find-node/{node_id}", headers=http_client_headers, timeout=timeout
        )
        return read_json_response(response)

    def transfer(self, node_id: str, data: bytes, port: int = 8081, timeout: int = 5):
        print(f"POST Transfer file to {node_id} ({self.container.name} - {self.node_id})")
        port = self.__external_port(port)
        response: requests.Response = requests.post(
            f"http://localhost:{port}/transfer-file/{node_id}", data, headers=http_client_headers, timeout=timeout
        )
        return read_json_response(response)


class LoggerJob(threading.Thread):
    compose_client: DockerClient

    def __init__(self, compose_client: DockerClient):
        super(LoggerJob, self).__init__()
        # self._stop = threading.Event()
        self.compose_client = compose_client

    def run(self, *args, **kwargs):
        try:
            for src, line in self.compose_client.compose.logs([], follow=True, stream=True, timestamps=False):
                line = line.decode("utf-8")
                file = sys.stdout if src == "stdout" else sys.stderr
                print(f"> {line}", end="", file=file)

        except BaseException as error:
            print(f"Logger failed: {error}")
        print(f"Logger stopped")

    # def stop(self):
    #     pass
    #     self._stop.set()


class Cluster:
    docker_client: DockerClient
    logger_job: LoggerJob

    def __init__(self, compose_client: DockerClient):
        self.docker_client = compose_client
        self.logger_job = LoggerJob(compose_client)

    def __del__(self):
        pass
        ## TODO
        # self.logger_job.stop()

    def start(self, clients: int, servers: int):
        print(f"Docker Compose Up (clients: {clients}, servers: {servers})")
        scales = {"client": clients, "relay_server": servers}
        self.docker_client.compose.up(detach=True, wait=True, scales=scales)
        self.logger_job.start()
        assert len(self.clients()) == clients
        assert len(self.servers()) == servers

    def __find_container(self, name_part: str) -> List[Container]:
        containers = []
        for container in self.docker_client.container.list():
            if name_part in container.name:
                containers.append(container)
        return containers

    def clients(self, name_part: str = "client") -> List[Client]:
        containers = self.__find_container(name_part)
        return [Client(container) for container in containers]

    def servers(self, name_part: str = "relay_server") -> List[Server]:
        containers = self.__find_container(name_part)
        return [Server(container) for container in containers]

    def disconnect(self, node: Node) -> Set[str]:
        networks = set()
        if isinstance(node.container.network_settings.networks, Dict):
            for network in node.container.network_settings.networks:
                print(f"Disconnecting {node} from {network}")
                self.docker_client.network.disconnect(network, node.container)
                networks.add(network)
        return networks

    def connect(self, node: Node, networks: Set[str]):
        for network in networks:
            print(f"Connecting {node} to {network}")
            self.docker_client.network.connect(network, node.container)


def set_netem(node: Node, latency="0ms"):
    print(f"Set netem for '{node.container.name}', latency {latency}")
    docker.execute(
        container=node.container,
        command=["tc", "qdisc", "replace", "dev", "eth0", "root", "netem", "delay", latency],
        privileged=True,
    )
