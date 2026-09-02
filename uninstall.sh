#!/usr/bin/env bash
set -euo pipefail

service_name="nut-power-bridge.service"
module_name="nut_power"

if (( EUID != 0 )); then
    echo "Run this uninstaller as root: sudo ./uninstall.sh" >&2
    exit 1
fi

for required_command in systemctl modprobe depmod find rm; do
    if ! command -v "${required_command}" >/dev/null 2>&1; then
        echo "Missing required command: ${required_command}" >&2
        exit 1
    fi
done

if systemctl is-active --quiet "${service_name}"; then
    systemctl stop "${service_name}"
fi

if systemctl is-active --quiet "${service_name}"; then
    echo "Could not stop ${service_name}; no files were removed." >&2
    exit 1
fi

systemctl disable "${service_name}" >/dev/null 2>&1 || true

if [[ -d "/sys/module/${module_name}" ]]; then
    modprobe -r "${module_name}"
fi

declare -A affected_kernel_versions=()
while IFS= read -r -d '' module_path; do
    relative_module_path="${module_path#/lib/modules/}"
    kernel_version="${relative_module_path%%/*}"
    affected_kernel_versions["${kernel_version}"]=1
    echo "Removing ${module_path}"
    rm -f -- "${module_path}"
done < <(
    find /lib/modules -mindepth 3 -maxdepth 3 -type f \
        \( -path '*/extra/nut_power.ko' \
        -o -path '*/extra/nut_power.ko.xz' \
        -o -path '*/extra/nut_power.ko.zst' \
        -o -path '*/extra/nut_power.ko.gz' \) \
        -print0
)

rm -f -- \
    /usr/local/sbin/nut-power-bridge \
    /etc/systemd/system/nut-power-bridge.service \
    /etc/systemd/system/multi-user.target.wants/nut-power-bridge.service \
    /etc/modules-load.d/nut-power-bridge.conf \
    /etc/default/nut-power-bridge

for kernel_version in "${!affected_kernel_versions[@]}"; do
    depmod -a "${kernel_version}"
done

systemctl daemon-reload
systemctl reset-failed "${service_name}" >/dev/null 2>&1 || true

echo "nut-power-bridge was removed successfully."
