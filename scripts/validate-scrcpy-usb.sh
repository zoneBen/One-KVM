#!/usr/bin/env bash
# One-KVM scrcpy-over-USB 板端验证脚本
#
# 只做验证，不修改系统配置：
#   - 不重启板子、不停止 one-kvm、不切换 USB 角色、不重绑 gadget
#   - 只会在手机上做输入注入、录屏、adb 隧道和可选的 adb reboot
#
# 用法:
#   scp scripts/validate-scrcpy-usb.sh root@<板子IP>:/tmp/
#   ssh root@<板子IP> 'bash /tmp/validate-scrcpy-usb.sh'
#
#   长稳:
#   bash /tmp/validate-scrcpy-usb.sh --load-seconds 3600
#
set -uo pipefail

[ -n "${BASH_VERSION:-}" ] || { echo "请用 bash 运行: bash $0"; exit 1; }

SERIAL=""
LOAD_SECONDS=60
NO_LOAD=0
DO_REBOOT=0
OUT_DIR=""

usage() {
  cat <<'EOF'
用法: sudo bash validate-scrcpy-usb.sh [选项]

选项:
  --serial SERIAL       指定 adb 序列号（多台设备时必填）
  --load-seconds N      共存负载测试时长，默认 60 秒（长稳可设 3600）
  --no-load             跳过负载测试，只做信息采集和能力自检
  --out DIR             报告目录，默认 /tmp/onekvm-scrcpy-validate-<时间>
  --reboot-phone        额外执行一次 adb reboot 并等待恢复
  -h, --help            显示帮助

说明:
  - 需要 root（adb 访问 USB、dmesg）
  - 负载测试期间请保持 Web UI 视频打开，并观察被控 PC 键鼠
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --serial) SERIAL="${2:-}"; shift 2 ;;
    --load-seconds) LOAD_SECONDS="${2:-60}"; shift 2 ;;
    --no-load) NO_LOAD=1; shift ;;
    --out) OUT_DIR="${2:-}"; shift 2 ;;
    --reboot-phone) DO_REBOOT=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "未知参数: $1"; usage; exit 2 ;;
  esac
done

if [ "$(id -u)" -ne 0 ]; then
  echo "请用 root 运行（adb 访问 USB、dmesg 需要）"
  exit 1
fi
command -v adb >/dev/null 2>&1 || { echo "未找到 adb，请先安装（apt install adb）"; exit 1; }

TS=$(date +%Y%m%d-%H%M%S)
OUT_DIR="${OUT_DIR:-/tmp/onekvm-scrcpy-validate-$TS}"
mkdir -p "$OUT_DIR" || exit 1
REPORT="$OUT_DIR/report.txt"

# 同时输出到终端和报告文件
exec > >(tee "$REPORT") 2>&1

ADB=(adb)
[ -n "$SERIAL" ] && ADB+=( -s "$SERIAL" )

DMESG_PID=""
MON_PID=""
cleanup() {
  [ -n "$DMESG_PID" ] && kill "$DMESG_PID" 2>/dev/null || true
  [ -n "$MON_PID" ] && kill "$MON_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

section() { printf '\n\n===== %s =====\n' "$*"; }
kv() { printf '%-32s %s\n' "$1:" "$2"; }
adb_x() { timeout 20 "${ADB[@]}" "$@"; }

list_authorized() { adb devices | awk '$2=="device" {print $1}'; }
count_authorized() { list_authorized | grep -c . || true; }

select_device() {
  local serials count
  serials="$(list_authorized)"
  if [ -n "$SERIAL" ]; then
    if printf '%s\n' "$serials" | grep -qx "$SERIAL"; then
      return 0
    fi
    echo "设备 $SERIAL 不在已授权列表中:"
    adb devices -l
    return 1
  fi
  count=$(printf '%s\n' "$serials" | grep -c . || true)
  if [ "$count" -eq 0 ]; then
    return 1
  elif [ "$count" -gt 1 ]; then
    echo "检测到多台已授权设备，请用 --serial 指定:"
    adb devices -l
    return 1
  fi
  SERIAL=$(printf '%s\n' "$serials" | head -n1)
  ADB=(adb -s "$SERIAL")
  echo "使用设备: $SERIAL"
  return 0
}

phone_usb_path() {
  adb devices -l | awk -v s="$SERIAL" '
    $1==s { for (i=1; i<=NF; i++) if ($i ~ /^usb:/) { sub(/^usb:/, "", $i); print $i } }
  ' | head -n1
}

RES_DEVICE="FAIL"
RES_INPUT="UNKNOWN"
RES_MP4="FAIL"
RES_RAW="WARN"
RES_TUNNEL="FAIL"
RES_LOAD="SKIP"
RES_BATTERY="UNKNOWN"
MP4_SIZE=0
RAW_SIZE=0
RAW_RC=1
REV=1
FWD=1
PHONE_USB_PATH=""

# ---------------------------------------------------------------------------
section "0. 基本信息"
date
uname -a
cat /proc/device-tree/model 2>/dev/null; echo
cat /etc/os-release 2>/dev/null
kv "adb" "$(adb version 2>&1 | tr '\n' ' ')"
kv "one-kvm" "$(systemctl is-active one-kvm 2>/dev/null || true)"

# ---------------------------------------------------------------------------
section "1. 设备选择与授权"
adb devices -l
if ! select_device; then
  echo
  echo "请确认：手机已连接、已开启 USB 调试、手机屏幕上已允许本机调试。"
  read -r -p "处理后按回车重试（Ctrl-C 退出）: " _ || true
  if ! select_device; then
    echo "仍未找到已授权设备，退出。"
    exit 1
  fi
fi
RES_DEVICE="PASS"
adb devices -l

# ---------------------------------------------------------------------------
section "2. USB 拓扑"
if command -v lsusb >/dev/null 2>&1; then
  echo "--- lsusb ---"; lsusb
  echo "--- lsusb -t ---"; lsusb -t
else
  echo "lsusb 未安装（apt install usbutils）"
fi

echo "--- UDC ---"
ls -l /sys/class/udc 2>/dev/null
for u in /sys/class/udc/*; do
  [ -e "$u" ] && echo "$(basename "$u"): state=$(cat "$u/state" 2>/dev/null)"
done

echo "--- gadget ---"
for g in /sys/kernel/config/usb_gadget/*; do
  [ -e "$g" ] && echo "$(basename "$g"): UDC=$(cat "$g/UDC" 2>/dev/null)"
done

echo "--- USB sysfs ---"
for d in /sys/bus/usb/devices/*; do
  [ -f "$d/idVendor" ] || continue
  printf '%s %s:%s speed=%s product=%s\n' \
    "$(basename "$d")" "$(cat "$d/idVendor")" "$(cat "$d/idProduct")" \
    "$(cat "$d/speed" 2>/dev/null)" "$(cat "$d/product" 2>/dev/null)"
done

echo "--- Type-C / usb_role ---"
for p in /sys/class/typec/port*; do
  [ -e "$p" ] || continue
  echo "== $(basename "$p")"
  for f in power_role data_role vconn_source orientation; do
    echo "  $f=$(cat "$p/$f" 2>/dev/null)"
  done
done
for r in /sys/class/usb_role/*; do
  [ -e "$r" ] && echo "$(basename "$r"): role=$(cat "$r/role" 2>/dev/null)"
done

echo "--- power_supply ---"
for f in /sys/class/power_supply/*/uevent; do
  [ -e "$f" ] || continue
  echo "== $f"
  grep -E 'POWER_SUPPLY_(ONLINE|STATUS|TYPE|VOLTAGE_NOW|CURRENT_NOW|USB_TYPE)' "$f" 2>/dev/null || cat "$f"
done

echo "--- dmesg baseline ---"
dmesg 2>/dev/null | grep -iE 'usb|dwc3|xhci|ehci|ohci|typec|fusb302|role|over.?current' | tail -n 200

echo "--- kernel config ---"
CFG=""
for f in /proc/config.gz "/boot/config-$(uname -r)" "/lib/modules/$(uname -r)/build/.config"; do
  [ -r "$f" ] && CFG="$f" && break
done
if [ -n "$CFG" ]; then
  case "$CFG" in
    *.gz) gzip -dc "$CFG" ;;
    *) cat "$CFG" ;;
  esac | grep -E 'CONFIG_USB_(XHCI|DWC3|EHCI|OHCI|GADGET|CONFIGFS|LIBCOMPOSITE|ROLE_SWITCH|STORAGE)|CONFIG_TYPEC|CONFIG_EXTCON'
else
  echo "no kernel config readable"
fi

# ---------------------------------------------------------------------------
section "3. 手机信息"
kv "serial" "$SERIAL"
for prop in ro.product.manufacturer ro.product.model ro.build.version.release \
            ro.build.version.sdk ro.build.version.security_patch; do
  printf '%-32s %s\n' "$prop:" "$(adb_x shell getprop "$prop" 2>/dev/null | tr -d '\r')"
done
printf '%-32s %s\n' "sys.usb.config:" "$(adb_x shell getprop sys.usb.config 2>/dev/null | tr -d '\r')"
printf '%-32s %s\n' "sys.usb.state:" "$(adb_x shell getprop sys.usb.state 2>/dev/null | tr -d '\r')"
printf '%-32s %s\n' "SELinux:" "$(adb_x shell getenforce 2>/dev/null | tr -d '\r')"
printf '%-32s %s\n' "app_process:" "$(adb_x shell which app_process 2>/dev/null | tr -d '\r')"
printf '%-32s %s\n' "screenrecord:" "$(adb_x shell which screenrecord 2>/dev/null | tr -d '\r')"
printf '%-32s %s\n' "wm size:" "$(adb_x shell wm size 2>/dev/null | tr -d '\r' | tr '\n' ' ')"
printf '%-32s %s\n' "wm density:" "$(adb_x shell wm density 2>/dev/null | tr -d '\r' | tr '\n' ' ')"
printf '%-32s %s\n' "adb_enabled:" "$(adb_x shell settings get global adb_enabled 2>/dev/null | tr -d '\r')"
printf '%-32s %s\n' "developer_options:" "$(adb_x shell settings get global development_settings_enabled 2>/dev/null | tr -d '\r')"
printf '%-32s %s\n' "usb.host feature:" "$(adb_x shell pm list features 2>/dev/null | grep -i 'usb.host' | tr '\n' ' ')"

PHONE_USB_PATH="$(phone_usb_path)"
echo
echo "--- 手机 USB 路径 ---"
kv "usb path" "${PHONE_USB_PATH:-<none>}"
if [ -n "$PHONE_USB_PATH" ] && [ -d "/sys/bus/usb/devices/$PHONE_USB_PATH" ]; then
  d="/sys/bus/usb/devices/$PHONE_USB_PATH"
  for f in idVendor idProduct speed product manufacturer serial bMaxPower; do
    [ -f "$d/$f" ] && echo "  $f=$(cat "$d/$f")"
  done
  echo "  sysfs: $(readlink -f "$d")"
  VID=$(cat "$d/idVendor" 2>/dev/null)
  PID=$(cat "$d/idProduct" 2>/dev/null)
  if command -v lsusb >/dev/null 2>&1 && [ -n "${VID:-}" ] && [ -n "${PID:-}" ]; then
    echo "--- lsusb -v 关键字段 ---"
    timeout 20 lsusb -v -d "$VID:$PID" 2>/dev/null \
      | grep -E 'bMaxPower|bInterfaceClass|iProduct|bcdUSB' | head -n 40
  fi
fi

# ---------------------------------------------------------------------------
section "4. 输入注入"
INPUT_FAIL=0
adb_x shell input keyevent KEYCODE_WAKEUP; RC=$?; echo "WAKEUP exit=$RC"; [ "$RC" -eq 0 ] || INPUT_FAIL=1
sleep 1
adb_x shell input keyevent KEYCODE_HOME; RC=$?; echo "HOME exit=$RC"; [ "$RC" -eq 0 ] || INPUT_FAIL=1
sleep 1
adb_x shell input tap 500 500; RC=$?; echo "TAP exit=$RC"; [ "$RC" -eq 0 ] || INPUT_FAIL=1
sleep 1
adb_x shell input text onekvm; RC=$?; echo "TEXT exit=$RC"; [ "$RC" -eq 0 ] || INPUT_FAIL=1
echo ">>> 请观察手机屏幕：是否亮屏/回到桌面/有触摸和文本输入"
if [ "$INPUT_FAIL" -eq 0 ]; then RES_INPUT="PASS"; else RES_INPUT="FAIL"; fi

# ---------------------------------------------------------------------------
section "5. MediaCodec / screenrecord"
adb_x shell rm -f /data/local/tmp/onekvm-test.mp4
timeout 30 "${ADB[@]}" shell screenrecord --time-limit 5 /data/local/tmp/onekvm-test.mp4
MP4_RC=$?
sleep 2
echo "screenrecord mp4 exit=$MP4_RC"
adb_x shell ls -l /data/local/tmp/onekvm-test.mp4
timeout 30 "${ADB[@]}" pull /data/local/tmp/onekvm-test.mp4 "$OUT_DIR/phone-screenrecord.mp4" >/dev/null 2>&1
PULL_RC=$?
MP4_SIZE=$(stat -c%s "$OUT_DIR/phone-screenrecord.mp4" 2>/dev/null || echo 0)
echo "mp4 size=$MP4_SIZE pull_exit=$PULL_RC"
[ "$MP4_SIZE" -gt 1000 ] 2>/dev/null && RES_MP4="PASS" || RES_MP4="FAIL"

echo "--- raw h264 ---"
timeout 40 "${ADB[@]}" exec-out screenrecord --output-format=h264 --time-limit 5 - \
  > "$OUT_DIR/phone-raw.h264" 2> "$OUT_DIR/phone-raw.err"
RAW_RC=$?
RAW_SIZE=$(stat -c%s "$OUT_DIR/phone-raw.h264" 2>/dev/null || echo 0)
echo "raw h264 exit=$RAW_RC size=$RAW_SIZE"
if [ -s "$OUT_DIR/phone-raw.err" ]; then
  echo "stderr:"
  head -n 20 "$OUT_DIR/phone-raw.err"
fi
if [ "$RAW_SIZE" -gt 1000 ] 2>/dev/null && [ "$RAW_RC" -eq 0 ]; then
  RES_RAW="PASS"
else
  RES_RAW="WARN"
fi
adb_x shell rm -f /data/local/tmp/onekvm-test.mp4

# ---------------------------------------------------------------------------
section "6. ADB 隧道（scrcpy 必需）"
adb_x reverse tcp:27183 tcp:27183; REV=$?
adb_x forward tcp:27183 tcp:27183; FWD=$?
echo "reverse exit=$REV forward exit=$FWD"
adb_x reverse --remove tcp:27183 >/dev/null 2>&1 || true
adb_x forward --remove tcp:27183 >/dev/null 2>&1 || true
if [ "$REV" -eq 0 ] || [ "$FWD" -eq 0 ]; then
  RES_TUNNEL="PASS"
else
  RES_TUNNEL="FAIL"
fi

# ---------------------------------------------------------------------------
section "7. 电池/充电（空载）"
BATTERY_OUT="$(adb_x shell dumpsys battery 2>&1 | tr -d '\r')"
echo "$BATTERY_OUT"
if printf '%s\n' "$BATTERY_OUT" | grep -qi 'level'; then
  RES_BATTERY="PASS"
else
  RES_BATTERY="WARN"
fi
adb_x shell dumpsys thermalservice 2>/dev/null | head -n 60

# ---------------------------------------------------------------------------
if [ "$NO_LOAD" -eq 1 ]; then
  section "8. 共存负载测试"
  echo "已跳过 (--no-load)"
else
  section "8. 共存负载测试 (${LOAD_SECONDS}s)"
  echo ">>> 请保持 Web UI 视频打开，并观察被控 PC 键鼠是否正常"
  echo ">>> 开始: $(date)"
  adb devices -l
  command -v lsusb >/dev/null 2>&1 && lsusb -t | sed -n '1,60p' || true

  DEADLINE=$(( $(date +%s) + LOAD_SECONDS ))

  dmesg -w > "$OUT_DIR/dmesg-during-load.log" 2>&1 &
  DMESG_PID=$!

  (
    while [ "$(date +%s)" -lt "$DEADLINE" ]; do
      printf -- '--- %s ---\n' "$(date)"
      timeout 10 adb devices -l 2>&1
      timeout 10 "${ADB[@]}" shell dumpsys battery 2>&1 \
        | grep -E 'AC powered|USB powered|status|level|voltage|temperature|current' || true
      sleep 10
    done
  ) > "$OUT_DIR/phone-monitor.log" 2>&1 &
  MON_PID=$!

  if [ "$RES_RAW" = "PASS" ] && [ "$RAW_RC" -eq 0 ]; then
    LOAD_MODE="raw_h264"
    while [ "$(date +%s)" -lt "$DEADLINE" ]; do
      timeout 40 "${ADB[@]}" exec-out screenrecord --output-format=h264 --time-limit 20 - \
        > /dev/null 2>> "$OUT_DIR/load-errors.log" \
        || echo "load fail $(date)" >> "$OUT_DIR/load-errors.log"
      sleep 1
    done
  else
    LOAD_MODE="mp4"
    while [ "$(date +%s)" -lt "$DEADLINE" ]; do
      timeout 60 "${ADB[@]}" shell \
        'screenrecord --time-limit 20 /data/local/tmp/onekvm-load.mp4; rm -f /data/local/tmp/onekvm-load.mp4' \
        2>> "$OUT_DIR/load-errors.log" \
        || echo "load fail $(date)" >> "$OUT_DIR/load-errors.log"
      sleep 1
    done
  fi

  kill "$MON_PID" 2>/dev/null || true; wait "$MON_PID" 2>/dev/null || true; MON_PID=""
  kill "$DMESG_PID" 2>/dev/null || true; wait "$DMESG_PID" 2>/dev/null || true; DMESG_PID=""
  adb_x shell rm -f /data/local/tmp/onekvm-load.mp4 >/dev/null 2>&1 || true

  echo ">>> 结束: $(date) (load_mode=$LOAD_MODE)"

  echo "--- 负载后 adb ---"
  adb devices -l
  if adb devices | awk -v s="$SERIAL" '$1==s && $2=="device" {found=1} END {exit !found}'; then
    echo "phone still authorized: yes"
    ADB_AFTER="yes"
  else
    echo "phone still authorized: NO"
    ADB_AFTER="no"
  fi

  command -v lsusb >/dev/null 2>&1 && { echo "--- 负载后 lsusb -t ---"; lsusb -t | sed -n '1,60p'; } || true
  echo "--- 负载后电池 ---"; adb_x shell dumpsys battery
  echo "--- 负载后温度 ---"; adb_x shell dumpsys thermalservice 2>/dev/null | head -n 40

  echo "--- dmesg 错误过滤 ---"
  grep -iE 'over.?current|reset|disconnect|device descriptor read|error -' \
    "$OUT_DIR/dmesg-during-load.log" 2>/dev/null | tail -n 80 || true

  OFFLINE_COUNT=$(grep -cE 'offline|unauthorized|no permissions' "$OUT_DIR/phone-monitor.log" 2>/dev/null || true)
  DMESG_ERR_COUNT=$(grep -icE 'over.?current|reset|device descriptor read|error -' "$OUT_DIR/dmesg-during-load.log" 2>/dev/null || true)
  echo "adb 掉线事件数: ${OFFLINE_COUNT:-0}"
  echo "dmesg 可疑错误数: ${DMESG_ERR_COUNT:-0}"

  if [ "$ADB_AFTER" = "yes" ] && [ "${OFFLINE_COUNT:-0}" -eq 0 ] && [ "${DMESG_ERR_COUNT:-0}" -eq 0 ]; then
    RES_LOAD="PASS"
  elif [ "$ADB_AFTER" = "no" ]; then
    RES_LOAD="FAIL"
  else
    RES_LOAD="WARN"
  fi

  echo "--- load 错误 ---"
  if [ -s "$OUT_DIR/load-errors.log" ]; then tail -n 40 "$OUT_DIR/load-errors.log"; else echo "(none)"; fi
fi

# ---------------------------------------------------------------------------
if [ "$DO_REBOOT" -eq 1 ]; then
  section "9. 手机重启恢复"
  START=$(date +%s)
  "${ADB[@]}" reboot || true
  echo "等待手机重新上线（最多 180 秒）..."
  for _ in $(seq 1 90); do
    if adb devices | awk '$2=="device" {found=1} END {exit !found}'; then break; fi
    sleep 2
  done
  "${ADB[@]}" wait-for-device 2>/dev/null || true
  END=$(date +%s)
  echo "恢复耗时约 $((END-START)) 秒"
  adb devices -l
  adb_x shell getprop sys.boot_completed
  adb_x shell getprop ro.product.model
  echo "--- 重启后 dmesg ---"
  dmesg 2>/dev/null | grep -iE 'usb 5-|xhci|over.?current|reset' | tail -n 40
fi

# ---------------------------------------------------------------------------
section "10. 汇总"
printf '%-28s %s\n' "adb 设备/授权" "$RES_DEVICE"
printf '%-28s %s\n' "输入注入" "$RES_INPUT"
printf '%-28s %s\n' "screenrecord mp4" "$RES_MP4"
printf '%-28s %s\n' "screenrecord raw h264" "$RES_RAW"
printf '%-28s %s\n' "adb reverse/forward" "$RES_TUNNEL"
printf '%-28s %s\n' "共存负载测试" "$RES_LOAD"
printf '%-28s %s\n' "电池信息" "$RES_BATTERY"
echo
echo "报告目录: $OUT_DIR"
echo "报告文件: $REPORT"
echo "dmesg 负载日志: $OUT_DIR/dmesg-during-load.log"
echo "手机监控日志: $OUT_DIR/phone-monitor.log"

TARBALL="/tmp/onekvm-scrcpy-report-$TS.tar.gz"
if tar czf "$TARBALL" -C "$(dirname "$OUT_DIR")" "$(basename "$OUT_DIR")" 2>/dev/null; then
  echo "打包文件: $TARBALL"
else
  echo "打包失败，请直接取走报告目录"
fi

echo
echo ">>> 脚本未覆盖，请手工确认："
echo "  1) 负载期间 Web UI 视频是否稳定、被控 PC 键鼠是否正常"
echo "  2) 拔插手机后视频/键鼠/adb 是否恢复"
echo "  3) 在 Web UI 改一次 HID 配置，手机是否仍在线"
echo "  4) 重启板子后 gadget/视频/手机 adb 是否自动恢复"
echo "  5) 锁屏/解锁后 adb 和 input 是否仍可用"
echo "  6) 手机开 USB 网络共享后 ip route 是否被改（测完关闭）"
echo "  7) 可选：USB 电流表 / 有源 Hub 对比供电"

if [ "$RES_DEVICE" = "FAIL" ] || [ "$RES_MP4" = "FAIL" ] || [ "$RES_TUNNEL" = "FAIL" ] || [ "$RES_LOAD" = "FAIL" ]; then
  echo
  echo ">>> 存在关键项失败，请把报告目录发回分析。"
  exit 1
fi
echo
echo ">>> 关键项通过，请把报告目录/打包文件发回，并补手工确认项。"
exit 0
