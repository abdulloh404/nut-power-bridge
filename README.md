# nut-power-bridge

Expose UPS data from NUT through the Linux `power_supply` subsystem so UPower and GNOME can read it, while `usbhid-ups` remains the sole owner of the USB device.

```text
APC UPS -> usbhid-ups -> upsd -> Rust bridge -> nut_power.ko
                                                |-- nut-battery
                                                `-- nut-ac
                                                     -> UPower -> GNOME
```

The initial version publishes five values:

| NUT | Linux `power_supply` |
| --- | --- |
| `battery.charge` | `CAPACITY` |
| `battery.voltage` | `VOLTAGE_NOW` (microvolts) |
| `battery.runtime` | `TIME_TO_EMPTY_NOW` (seconds) and energy/power values for UPower |
| `ups.status` | battery `STATUS` |
| `OL`/`ONLINE` and `OB`/`ONBAT`/`ONBATT` in `ups.status` | AC `ONLINE` |

`CHRG` maps to `Charging`, while `DISCHRG` or `OB`/`ONBAT`/`ONBATT` maps to `Discharging`. `OL`/`ONLINE` without `CHRG` maps to `Full` at 100% capacity, or `Not charging` otherwise.

The module registers `nut-battery` and `nut-ac` as soon as it loads, with an initial status of `Unknown`, AC online, and an initial capacity that prevents UPower from treating the battery as critically low. The daemon replaces these values as soon as it reads the first snapshot from NUT. This keeps the devices visible in UPower even if `upsd` starts late or temporarily disconnects.

## Requirements

- NUT `upsd` already reading the UPS through `usbhid-ups`
- Rust toolchain (`cargo`)
- `make`, a compiler, and kernel headers matching the running kernel
- UPower to provide power information to the desktop

On Ubuntu/Debian, install the build tools and kernel headers with:

```bash
sudo apt install build-essential linux-headers-$(uname -r) cargo
```

## Build and install

Build the daemon and kernel module:

```bash
make
```

Install the binary, module, and systemd service:

```bash
sudo ./install.sh
```

The installer loads `nut_power`, enables `nut-power-bridge.service`, and installs the module for the running kernel. Rebuild and reinstall the module when switching to a different kernel.

If `nut_power` is already loaded when reinstalling, the installer does not unload the active module. Reboot or reload the module later to use the newly installed version.

## Configuration

Edit `/etc/default/nut-power-bridge` if the defaults do not match your NUT setup:

```ini
NUT_HOST=127.0.0.1:3493
NUT_UPS_NAME=ups
NUT_POLL_INTERVAL_SECONDS=2
NUT_TIMEOUT_SECONDS=5
NUT_SYSFS_PATH=/sys/kernel/nut_battery/update
```

Then have the system administrator apply the updated configuration using the system's usual service management procedure.

The daemon communicates directly with the NUT text protocol over TCP instead of invoking `upsc` on every poll. It reconnects automatically if the connection to `upsd` is lost. `battery.charge` and `ups.status` are required. The optional `battery.runtime` and `battery.voltage` values fall back to their last known values, or `0` if the UPS does not support them and no previous values are available.

If the connection is lost after receiving data, the bridge retains the last known percentage, runtime, voltage, and AC status, while setting the battery status to `Unknown` until it successfully reads a new snapshot.

For compatibility with UPower 0.99.17, the module exposes a normalized full energy value of 100 Wh, calculates the current energy from the battery percentage, and derives power from `battery.runtime`. This ratio allows UPower to display a `time to empty` matching NUT while the status is `Discharging`. These energy and power values are used only to calculate time estimates; they are not the UPS's actual energy or power measurements.

## Inspect values

After installation, the kernel exposes values at:

```text
/sys/class/power_supply/nut-battery/
/sys/class/power_supply/nut-ac/
```

Example commands to inspect the values:

```bash
cat /sys/class/power_supply/nut-battery/capacity
cat /sys/class/power_supply/nut-battery/voltage_now
cat /sys/class/power_supply/nut-battery/time_to_empty_now
cat /sys/class/power_supply/nut-battery/status
cat /sys/class/power_supply/nut-ac/online
upower -e
upower -i /org/freedesktop/UPower/devices/battery_nut_battery
upower -i /org/freedesktop/UPower/devices/line_power_nut_ac
upower -d
```

When NUT reports `OB`/`ONBAT`/`ONBATT`, `upower -d` should show the battery as `discharging`, line power as `online: no`, and `on-battery: yes`. When NUT reports `OL`/`ONLINE`, line power should show `online: yes`; `CHRG` should show the battery as `charging`.

Bridge logs are available in the system journal under `nut-power-bridge.service`.

## Uninstall

Stop and disable the service, unload the kernel module, and remove the binary, systemd unit, configuration, and modules installed for all kernel versions:

```bash
sudo ./uninstall.sh
```

This command does not remove the source code in the repository or erase the system journal.
