from python_on_whales import docker, DockerClient, Container
from datetime import datetime, timedelta
from typing import Dict, List, TypeVar
import json
import re
import requests

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
        raise Exception(response.content)


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

    def address(self, net_name_part="relay-network") -> str | None:  # type: ignore
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

    def ping(self, node_id: str, port: int = 8081):
        port = self.ports()[f"{port}/tcp"]
        response: requests.Response = requests.get(
            f"http://localhost:{port}/ping/{node_id}", headers=http_client_headers
        )
        return read_json_response(response)

    def sessions(self, port: int = 8081):
        port = self.ports()[f"{port}/tcp"]
        response: requests.Response = requests.get(f"http://localhost:{port}/sessions", headers=http_client_headers)
        return read_json_response(response)


class Cluster:
    docker_client: DockerClient

    def __init__(self, compose_client: DockerClient):
        self.docker_client = compose_client

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


def set_netem(node: Node, latency="0ms"):
    print(f"Set netem for '{node.container.name}', latency {latency}")
    docker.execute(
        container=node.container,
        command=["tc", "qdisc", "replace", "dev", "eth0", "root", "netem", "delay", latency],
        privileged=True,
    )
