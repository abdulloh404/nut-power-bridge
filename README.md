# nut-power-bridge

Expose UPS data from NUT through the Linux `power_supply` subsystem so UPower and GNOME can read it, while `usbhid-ups` remains the sole owner of the USB device.

```text
APC UPS -> usbhid-ups -> upsd -> Rust bridge -> nut_power.ko
                                                |-- nut-battery
                                                `-- nut-ac
                                                     -> UPower -> GNOME
```

The bridge publishes these values:

| NUT | Linux `power_supply` |
| --- | --- |
| `battery.charge` | `CAPACITY` |
| `battery.voltage` | `VOLTAGE_NOW` (microvolts) |
| `battery.runtime` | `TIME_TO_EMPTY_NOW` (seconds) and energy/power values for UPower |
| `ups.status` | battery `STATUS` |
| `OL`/`ONLINE` and `OB`/`ONBAT`/`ONBATT` in `ups.status` | AC `ONLINE` |
| UPS charging data, a learned charging rate, or a configured fallback | `TIME_TO_FULL_NOW` (seconds) and charging `POWER_NOW` |

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

If `nut_power` is already loaded when reinstalling, the installer does not unload the active module. Reboot or reload the module later to use the newly installed version. Charging-time support requires the new module: `/sys/kernel/nut_battery/protocol_version` must report `2`. Until then, the daemon continues sending the legacy five-field snapshots so percentage, status, and discharge runtime remain available. Rebooting after installation activates both the new module and daemon together.

## Configuration

Edit `/etc/default/nut-power-bridge` if the defaults do not match your NUT setup:

```ini
NUT_HOST=127.0.0.1:3493
NUT_UPS_NAME=ups
NUT_POLL_INTERVAL_SECONDS=2
NUT_TIMEOUT_SECONDS=5
NUT_SYSFS_PATH=/sys/kernel/nut_battery/update
NUT_CHARGE_FULL_SECONDS=14400
NUT_CHARGE_STATE_PATH=/var/lib/nut-power-bridge/charge-rate
NUT_TIME_TO_FULL_VAR=
```

Then have the system administrator apply the updated configuration using the system's usual service management procedure.

The installer preserves an existing configuration file. New settings use the defaults above unless explicitly overridden.

The daemon communicates directly with the NUT text protocol over TCP instead of invoking `upsc` on every poll. It reconnects automatically if the connection to `upsd` is lost. `battery.charge` and `ups.status` are required. The optional `battery.runtime` and `battery.voltage` values fall back to their last known values, or `0` if the UPS does not support them and no previous values are available.

If the connection is lost after receiving data, the bridge retains the last known percentage, runtime, voltage, and AC status, while setting the battery status to `Unknown` until it successfully reads a new snapshot.

For compatibility with UPower 0.99.17, the module exposes a normalized full energy value of 100 Wh, calculates the current energy from the battery percentage, and derives power from `battery.runtime`. This ratio allows UPower to display a `time to empty` matching NUT while the status is `Discharging`. These energy and power values are used only to calculate time estimates; they are not the UPS's actual energy or power measurements.

## Charging-time estimates

While the UPS reports `Charging`, AC is online, and capacity is below 100%, the daemon selects a charging estimate in this order:

1. **UPS data.** If `NUT_TIME_TO_FULL_VAR` names a NUT variable that actually reports seconds until fully charged, its positive numeric value takes priority. Leave this setting empty if the driver does not expose such a variable. Otherwise, if both `battery.capacity` (Ah) and `battery.current` (A) are available and valid, the daemon estimates the remaining time from their values and the remaining percentage. It uses the current magnitude only while the UPS explicitly reports Charging; this requires the values to describe the same battery bank and the current to represent battery charging current.
2. **Learned charging rate.** The daemon measures how long the percentage takes to increase while continuously charging. It discards the first partially observed percentage interval, then requires at least two percentage points over at least 60 seconds. New rates are smoothed with the previous learned rate. Discharging, missing data, reconnects, percentage decreases, abrupt jumps, and long observation gaps reset the observation window, not the last valid learned rate.
3. **Configured fallback.** `NUT_CHARGE_FULL_SECONDS` defines an assumed 0–100% charging duration. The default is `14400` seconds (4 hours), not a manufacturer specification. The initial remaining-time estimate is this duration multiplied by the remaining percentage. The allowed range is `1200`–`72000` seconds.

An estimate is therefore available from the first valid charging snapshot, even on a fresh installation with no history. It is an estimate, not a guarantee of when charging will finish; charge taper, load, and battery condition can change the actual duration. `battery.runtime` is used only for discharge runtime and must not be configured as a time-to-full variable.

The learned rate is saved to `NUT_CHARGE_STATE_PATH`, keyed to the configured NUT host and UPS name. The cache is retained across reboots, expires after 30 days, and is written atomically only when learning produces a new rate, with at least a minute between write attempts. Invalid or mismatched cache data is ignored. Cache I/O errors are logged without interrupting battery updates. After replacing the UPS or its battery, remove the old cache before the next daemon start to discard the previous charging profile.

The service creates `/var/lib/nut-power-bridge` with mode `0700`; cache files use `0600`. If you select a path outside this directory, also allow that path in the service's filesystem sandbox. The default state directory is removed by `uninstall.sh`; a custom state file outside it must be removed separately.

UPower 0.99.17 does not read the kernel's `time_to_full_now` attribute directly. While charging, the module also supplies a normalized power value consistent with the estimate:

```text
power_now (uW) = (energy_full - energy_now) (uWh) * 3600 / time_to_full (seconds)
```

UPower can then calculate `TimeToFull` from energy and power without reusing the previous discharge rate. Estimates are bounded to 20 hours and a minimum of 12 seconds per remaining percentage point, keeping normalized charging power at or below UPower's 300 W limit. The module also exposes `time_to_full_now` for direct inspection. Charging time is zero when fully charged, discharging, not charging, or unavailable. The bridge logs which source is selected when it changes.

At exactly 0% while charging, reference energy is floored at 0.101 Wh because UPower suppresses time estimates below 0.1 Wh. Reported percentage remains 0%, and charging power is calculated from the same reference energy so the time estimate remains consistent. This does not change discharge behavior.

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
cat /sys/class/power_supply/nut-battery/time_to_full_now
cat /sys/class/power_supply/nut-battery/power_now
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

Stop and disable the service, unload the kernel module, and remove the binary, systemd unit, configuration, default charging-rate state directory, and modules installed for all kernel versions:

```bash
sudo ./uninstall.sh
```

This command does not remove the source code in the repository or erase the system journal.
