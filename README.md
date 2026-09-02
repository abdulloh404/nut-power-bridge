# nut-power-bridge

เชื่อมข้อมูล UPS จาก NUT เข้ากับ Linux `power_supply` เพื่อให้ UPower และ GNOME อ่านค่าได้ โดย `usbhid-ups` ยังคงเป็นผู้ครอบครองอุปกรณ์ USB เพียงตัวเดียว

```text
APC UPS -> usbhid-ups -> upsd -> Rust bridge -> nut_power.ko
                                                |-- nut-battery
                                                `-- nut-ac
                                                     -> UPower -> GNOME
```

รุ่นแรกส่งข้อมูล 5 ค่า:

| NUT | Linux `power_supply` |
| --- | --- |
| `battery.charge` | `CAPACITY` |
| `battery.voltage` | `VOLTAGE_NOW` (microvolts) |
| `battery.runtime` | `TIME_TO_EMPTY_NOW` (seconds) และค่า energy/power สำหรับ UPower |
| `ups.status` | battery `STATUS` |
| `OL`/`ONLINE` และ `OB`/`ONBAT`/`ONBATT` ใน `ups.status` | AC `ONLINE` |

สถานะ `CHRG` เป็น `Charging`, `DISCHRG` หรือ `OB`/`ONBAT`/`ONBATT` เป็น `Discharging` และ `OL`/`ONLINE` ที่ไม่มี `CHRG` เป็น `Full` เมื่อความจุ 100% มิฉะนั้นเป็น `Not charging`

module สร้าง `nut-battery` และ `nut-ac` ตั้งแต่ถูกโหลด โดยเริ่มด้วยสถานะ `Unknown`, AC online และความจุเริ่มต้นที่ไม่ทำให้ UPower ตีความว่าแบตวิกฤต จากนั้น daemon จะเขียนค่าจริงทับทันทีที่อ่าน snapshot แรกจาก NUT สำเร็จ ทำให้อุปกรณ์ยังอยู่ใน UPower แม้ `upsd` จะเริ่มช้าหรือหลุดชั่วคราว

## สิ่งที่ต้องมี

- NUT `upsd` ที่อ่าน UPS ผ่าน `usbhid-ups` ได้แล้ว
- Rust toolchain (`cargo`)
- `make`, compiler และ kernel headers ที่ตรงกับ kernel ปัจจุบัน
- UPower สำหรับส่งข้อมูลต่อให้ desktop

บน Ubuntu/Debian kernel headers ปกติติดตั้งด้วย:

```bash
sudo apt install build-essential linux-headers-$(uname -r) cargo
```

## Build และติดตั้ง

Build daemon และ kernel module:

```bash
make
```

ติดตั้ง binary, module และ systemd service:

```bash
sudo ./install.sh
```

ตัวติดตั้งจะโหลด `nut_power`, เปิดใช้ `nut-power-bridge.service` และเก็บ module ไว้สำหรับ kernel ปัจจุบัน เมื่อเปลี่ยน kernel ต้อง build และติดตั้ง module ใหม่

หากติดตั้งทับขณะที่ `nut_power` ถูกโหลดอยู่ ตัวติดตั้งจะไม่ถอด module ที่กำลังใช้งาน ให้ reboot หรือ reload module ภายหลังเพื่อเริ่มใช้ไฟล์ module รุ่นใหม่

## ตั้งค่า

แก้ `/etc/default/nut-power-bridge` หากค่าเริ่มต้นไม่ตรงกับ NUT:

```ini
NUT_HOST=127.0.0.1:3493
NUT_UPS_NAME=ups
NUT_POLL_INTERVAL_SECONDS=2
NUT_TIMEOUT_SECONDS=5
NUT_SYSFS_PATH=/sys/kernel/nut_battery/update
```

จากนั้นให้ผู้ดูแลระบบนำ service กลับมาใช้ค่าชุดใหม่ตามขั้นตอนปกติของเครื่อง

daemon คุย NUT text protocol ผ่าน TCP โดยตรง ไม่ได้เรียก `upsc` ซ้ำทุก poll และจะ reconnect เองเมื่อ `upsd` หลุด `battery.charge` และ `ups.status` เป็นค่าหลักที่ต้องมี ส่วน `battery.runtime` และ `battery.voltage` จะใช้ค่าล่าสุด หรือ `0` หาก UPS รุ่นนั้นไม่รองรับ

เมื่อเคยรับข้อมูลแล้วแต่การเชื่อมต่อหลุด bridge จะคงเปอร์เซ็นต์, runtime, voltage และสถานะ AC ล่าสุดไว้ พร้อมเปลี่ยน battery status เป็น `Unknown` จนกว่าจะอ่าน snapshot ใหม่สำเร็จ

เพื่อรองรับ UPower 0.99.17 module จะ expose ค่า energy เต็มแบบ normalized 100 Wh, คำนวณ energy ปัจจุบันจากเปอร์เซ็นต์ และคำนวณ power จาก `battery.runtime` อัตราส่วนนี้ทำให้ UPower แสดง `time to empty` ตรงกับ NUT ขณะสถานะเป็น `Discharging`; ค่า energy/power ดังกล่าวใช้สำหรับคำนวณเวลา ไม่ใช่ค่าพลังงานจริงของ UPS

## ตรวจค่า

หลังติดตั้ง ค่าจาก kernel อยู่ที่:

```text
/sys/class/power_supply/nut-battery/
/sys/class/power_supply/nut-ac/
```

ตัวอย่างตรวจค่า:

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

เมื่อ NUT รายงาน `OB`/`ONBAT`/`ONBATT` ควรเห็น battery เป็น `discharging`, line power เป็น `online: no` และ `on-battery: yes` ใน `upower -d` เมื่อรายงาน `OL`/`ONLINE` ควรเห็น line power เป็น `online: yes`; `CHRG` จะแสดง `charging`

log ของ bridge อยู่ใน system journal ของ `nut-power-bridge.service`

## ถอนการติดตั้ง

หยุดและ disable service, unload kernel module และลบ binary, systemd unit, config รวมถึง module ที่เคยติดตั้งไว้ทุก kernel:

```bash
sudo ./uninstall.sh
```

คำสั่งนี้ไม่ลบ source code ใน repository และไม่ลบข้อมูลรวมใน system journal
