import pytest
from python_on_whales import DockerClient

@pytest.fixture(scope="session")
def compose_build():
    print("Docker Compose Build")
    docker = DockerClient(compose_files=["../test_env/docker-compose.yml"])
    docker.compose.build()
    return docker

@pytest.fixture(scope="function")
def compose_up(compose_build):
    def _compose_up(clients_num):
        print("Docker Compose Up (clients: {})".format(clients_num))
        scales = {"client": clients_num }
        compose_build.compose.up(detach=True, wait=True, scales=scales)
        return compose_build

    return _compose_up

@pytest.fixture(scope="function", autouse=True)
def compose_down(compose_build):
    yield # wait for test to finish
    print("Docker Compose Down")
    compose_build.compose.down()
