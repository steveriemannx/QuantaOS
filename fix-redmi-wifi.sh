#!/bin/bash
# 修复 Redmi 热点频繁掉线 / 必须插 USB 才能切换 Wi-Fi 的问题
# 用法： sudo bash fix-redmi-wifi.sh
set -uo pipefail

STA=${STA:-wlx0013ef2f2394}
log() { echo "[fix] $*"; }

# ---------------------------------------------------------------- 1. 关掉残留的 WiFi 中继栈
# 这套 create_ap + hostapd-relay + wifi-relay-* 服务在单张网卡上同时做 STA+AP，
# 会跟 NetworkManager 抢同一块 phy，并且 create_ap 里写的是早已不存在的
# INTERNET_IFACE=wlx60ee5c96dbd9，导致 2 万多次重启循环。
for u in create_ap.service hostapd-relay.service dnsmasq-relay.service \
         wifi-relay-address.service wifi-relay-watchdog.service wifi-relay-watchdog.timer; do
    systemctl disable --now "$u" >/dev/null 2>&1 || true
done
# prepare 的 ExecStop 会拆掉 ap0 vif 和 iptables NAT 规则
systemctl disable --now wifi-relay-prepare.service >/dev/null 2>&1 || true
systemctl mask create_ap.service hostapd-relay.service dnsmasq-relay.service >/dev/null 2>&1 || true
systemctl reset-failed >/dev/null 2>&1 || true
log "relay services disabled"

# ---------------------------------------------------------------- 2. 删掉遗留的 AP 虚拟接口
iw dev ap0 del 2>/dev/null || true
nmcli device set ap0 managed yes >/dev/null 2>&1 || true
log "stale ap0 removed"

# ---------------------------------------------------------------- 3. 监管域设为 CN
# country 00 时 2.4GHz 只有 6dBm、信道 13 是 NO-IR(不能做 AP)，既弱又容易掉。
iw reg set CN 2>/dev/null || true
cat > /etc/modprobe.d/cfg80211-regdom.conf <<'EOF'
options cfg80211 ieee80211_regdom=CN
EOF
log "regdomain -> CN"

# ---------------------------------------------------------------- 4. 关掉 MT7612U 的 USB 自动挂起
# mt76x2u 在 autosuspend 下很容易自己掉线；这是"经常断链"的主因之一。
cat > /etc/udev/rules.d/99-mt7612u-no-autosuspend.rules <<'EOF'
# MediaTek MT7612U: keep the adapter powered, autosuspend causes random disconnects
ACTION=="add", SUBSYSTEM=="usb", ATTR{idVendor}=="0e8d", ATTR{idProduct}=="7612", TEST=="power/control", ATTR{power/control}="on"
EOF
udevadm control --reload-rules
for d in /sys/bus/usb/devices/*/; do
    [ -f "$d/idVendor" ] || continue
    if [ "$(cat "$d/idVendor" 2>/dev/null)" = "0e8d" ] && \
       [ "$(cat "$d/idProduct" 2>/dev/null)" = "7612" ]; then
        echo on > "$d/power/control" 2>/dev/null || true
        log "autosuspend off for $d"
    fi
done

# ---------------------------------------------------------------- 4b. 关闭 mt76 USB scatter-gather
# 这是"延迟 2 秒 + 丢包"的真正元凶：mt76x2u 的 usb_sg 在这台机器上会卡住 TX 队列。
cat > /etc/modprobe.d/mt76-usb-sg.conf <<'EOF'
# MT7612U: USB scatter-gather causes TX stalls / huge latency on some hosts
options mt76_usb disable_usb_sg=1
EOF
if [ "$(cat /sys/module/mt76_usb/parameters/disable_usb_sg 2>/dev/null)" != "Y" ]; then
    modprobe -r mt76x2u mt76x02_usb mt76_usb 2>/dev/null || true
    modprobe mt76x2u 2>/dev/null || true
    sleep 2
    log "mt76x2u reloaded (disable_usb_sg=1)"
fi

# ---------------------------------------------------------------- 5. NetworkManager 关省电
rm -f /etc/NetworkManager/conf.d/default-wifi-powersave-on.conf
rm -f /etc/NetworkManager/conf.d/99-wifi-relay.conf
cat > /etc/NetworkManager/conf.d/wifi-powersave-off.conf <<'EOF'
[connection]
wifi.powersave = 2
EOF
systemctl reload NetworkManager >/dev/null 2>&1 || true
sleep 1
iw dev "$STA" set power_save off 2>/dev/null || true
log "wifi powersave off"

# ---------------------------------------------------------------- 6. 清理 create_ap 的失效配置
if [ -f /etc/create_ap.conf ]; then
    sed -i 's|^INTERNET_IFACE=.*|INTERNET_IFACE=enp8s0|' /etc/create_ap.conf
    log "create_ap.conf INTERNET_IFACE fixed"
fi

log "done"
