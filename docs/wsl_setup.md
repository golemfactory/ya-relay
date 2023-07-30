# WSL2 setup

`ya-relay` latency tests require enabling _Network emulator_ (NETEM) feature.

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
