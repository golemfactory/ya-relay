from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import time
import pytest
import logging
import numpy as np
from sklearn.linear_model import LinearRegression
import matplotlib.pyplot as plt


LOGGER = logging.getLogger(__name__)

file_num = 0


def ping_nodes(client0, clients):
    ping_times = []
    for client in clients:
        ping_response = client0.ping(client.node_id)
        LOGGER.debug(f"Ping duration {ping_response['duration']}ms")
        ping_times.append(ping_response["duration"])
    return ping_times


def calc(ping_times, plot=False):
    n = len(ping_times)
    mean = sum(ping_times) / n
    deviations = [(t - mean) ** 2 for t in ping_times]
    variance = sum(deviations) / n
    min_ping = min(ping_times)
    max_ping = max(ping_times)

    x = np.array(range(len(ping_times)))
    y = np.array(ping_times)
    x = x.reshape(-1, 1)
    regression_model = LinearRegression()

    regression_model.fit(x, y)
    y_pred = regression_model.predict(x)

    a = regression_model.coef_[0]
    b = regression_model.intercept_

    if plot:
        global file_num
        plt.clf()
        plt.plot(x, y, "o")
        plt.plot(x, y_pred, color="red")
        plt.savefig(f"plot-{file_num}.png")
        file_num += 1

    return (a, b, min_ping, max_ping, mean, variance)


@pytest.mark.skip(reason="Takes too much time to run in CI")
def test_many_pings(compose_up):
    cluster: Cluster = compose_up(250)

    server: Server = cluster.servers()[0]
    client0 = cluster.clients()[0]
    clients = cluster.clients()[1:]

    LOGGER.info(f"Start initial pings")
    ping_times_init = ping_nodes(client0, clients)

    clients = clients[0:5]

    LOGGER.info(f"Start first ping times measurement")
    ping_times_1 = ping_nodes(client0, clients)
    (a, b, min_ping, max_ping, mean, variance) = calc(ping_times_1, True)
    LOGGER.info(f"Mean ping time: {mean}ms")
    LOGGER.info(f"Min ping time: {min_ping}ms")
    LOGGER.info(f"Max ping time: {max_ping}ms")
    LOGGER.info(f"Variance: {variance}ms")
    LOGGER.info(f"Linear regression: a={a}, b={b}")

    assert abs(a) < 0.15  # TODO still assuming ping times might be increasing with time

    for client in clients:
        find_response = client0.find(client.node_id)
        # identities = find_response["node"]["identities"]

    LOGGER.info(f"Start second ping times measurement")
    ping_times_2 = ping_nodes(client0, clients)
    (a, b, min_ping, max_ping, mean, variance) = calc(ping_times_2, True)
    LOGGER.info(f"Mean ping time: {mean}ms")
    LOGGER.info(f"Min ping time: {min_ping}ms")
    LOGGER.info(f"Max ping time: {max_ping}ms")
    LOGGER.info(f"Variance: {variance}ms")
    LOGGER.info(f"Linear regression: a={a}, b={b}")

    assert abs(a) < 0.15  # TODO still assuming ping times might be increasing with time

    LOGGER.info(f"Done")
