import pytest
from python_on_whales import DockerClient
from utils import Cluster


@pytest.fixture(scope="package")
def compose_build() -> DockerClient:
    build_args = {"SERVER_LATENCY": "0ms", "CLIENT_LATENCY": "0ms"}
    return __compose_build(build_args)


def __compose_build(build_args) -> DockerClient:
    print("Docker Compose Build")
    __write_dot_env(build_args)
    docker = DockerClient(compose_files=["docker-compose.yml"])
    docker.compose.build(build_args=build_args)
    return docker


def __write_dot_env(properties):
    with open(".env", "w") as f:
        for key, value in properties.items():
            f.write("%s=%s\n" % (key, value))


@pytest.fixture(scope="function")
def compose_up(compose_build: DockerClient):
    def _compose_up(clients: int, servers: int = 1, build_args=None) -> Cluster:
        _compose_build = compose_build
        if build_args is not None:
            _compose_build = __compose_build(build_args)
        cluster = Cluster(_compose_build)
        cluster.start(clients, servers)
        return cluster

    return _compose_up


@pytest.fixture(scope="function", autouse=True)
def compose_down(compose_build: DockerClient):
    yield  # wait for test to finish
    print("Docker Compose Down")
    compose_build.compose.down()
