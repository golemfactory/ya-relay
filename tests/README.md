# Functional Tests

## Environment setup

Requirements:

- Docker
- _Linux Traffic Control_ (tc) with _Network Emulation_ (netem)

Python setup

```bash
# Tests use Python 3.10
# It can be installed using `virtualenv`
# Install it if missing
sudo apt-get install virtualenv

# Initialize virtualenv
virtualenv .venv --python=python3.10
```

Build and run

```bash
# Switch to Python 3.10
source .venv/bin/activate
# Install poetry
pip install poetry
# Install dependencies
poetry install
# Run tests
poetry run pytest
```

Test development checks

```bash
source .venv/bin/activate
# Assuming build and run was alredy performed
poetry run poe checks
```

### WSL2 Troubleshoot

`ya-relay` latency tests require enabling _Network Emulation_ (NETEM) feature.

To enable it necessary is to build boot image.

```bash
# Install necessary tools
sudo apt install build-essential flex bison dwarves libssl-dev libelf-dev

# Clone WSL2 linux kernel repository (tested on 5.15.90.4)
git clone --no-tags --no-recurse-submodules --depth=1 --branch linux-msft-wsl-5.15.90.4 git@github.com:microsoft/WSL2-Linux-Kernel.git
cd WSL2-Linux-Kernel;

# Configure build
make menuconfig
# Select `< Load >` and use path `arch/x86/configs/config-wsl`.
# Then go to
# `Networking support`` -> `Networking options` -> `QoS and/or fair queuing`
# and mark "Network emulator (NETEM)" as "<*>" (using Space key).
# Finally `< Save >` config as `my_config` and `< Exit >` from menu.

# Build boot image
make -j8 KCONFIG_CONFIG=my_config
```

Copy boot image from `WSL2-Linux-Kernel/arch/x86/boot/bzImage` in WSL2 filesystem into e.g. `C:\Users\%USERNAME%\bzImage`.

Configure boot image in `C:\Users\%USERNAME%\.wslconfig` (replace `USERNAME`):

```.wslconfig
[wsl2]
kernel=C:\\Users\\USERNAME\\bzImage
```

Run `wsl --shutdown` and start WSL again.
