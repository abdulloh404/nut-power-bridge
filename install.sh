#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
kernel_release="$(uname -r)"
kernel_module_dir="/lib/modules/${kernel_release}/extra"
daemon_binary="${project_dir}/daemon/target/release/nut-power-bridge"
kernel_module="${project_dir}/kernel/nut_power.ko"

if (( EUID != 0 )); then
    echo "Run this installer as root: sudo ./install.sh" >&2
    exit 1
fi

for required_command in install depmod modprobe systemctl; do
    if ! command -v "${required_command}" >/dev/null 2>&1; then
        echo "Missing required command: ${required_command}" >&2
        exit 1
    fi
done

if [[ ! -f "${daemon_binary}" || ! -f "${kernel_module}" ]]; then
    echo "Build artifacts are missing; run 'make' before sudo ./install.sh" >&2
    exit 1
fi

install -Dm0755 \
    "${daemon_binary}" \
    /usr/local/sbin/nut-power-bridge
install -Dm0644 \
    "${kernel_module}" \
    "${kernel_module_dir}/nut_power.ko"
install -Dm0644 \
    "${project_dir}/systemd/nut-power-bridge.service" \
    /etc/systemd/system/nut-power-bridge.service
install -Dm0644 \
    "${project_dir}/systemd/nut-power-bridge.modules-load" \
    /etc/modules-load.d/nut-power-bridge.conf

if [[ ! -e /etc/default/nut-power-bridge ]]; then
    install -Dm0644 \
        "${project_dir}/systemd/nut-power-bridge.default" \
        /etc/default/nut-power-bridge
fi

depmod -a "${kernel_release}"

if [[ ! -d /sys/module/nut_power ]]; then
    modprobe nut_power
else
    echo "nut_power is already loaded; the installed module will be used after the next reload or reboot."
fi

systemctl daemon-reload
systemctl enable nut-power-bridge.service
systemctl restart nut-power-bridge.service

echo "nut-power-bridge installed successfully."
