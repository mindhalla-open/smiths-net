#!/usr/bin/env bash
# Открыть порты для Megafon Multifon SIP trunk на этом хосте (ufw).
# Запуск: sudo bash examples/open-megafon-ports.sh

set -euo pipefail

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
  echo "Запустите с sudo: sudo bash $0" >&2
  exit 1
fi

echo "=== Текущие адреса ==="
ip -4 addr show scope global | grep -E 'inet ' || true
echo "Публичный IP (ifconfig.me): $(curl -s --max-time 5 ifconfig.me || echo '?')"
echo

echo "=== UFW: открываем SIP + RTP (узкий пул для TL-WR844N) ==="
ufw allow 5060/udp comment 'Megafon SIP UDP'
ufw allow 5060/tcp comment 'Megafon SIP TCP'
for p in $(seq 10000 10005); do
  ufw allow "${p}/udp" comment 'Megafon RTP'
done
ufw reload

echo
echo "=== UFW status ==="
ufw status numbered | grep -E '5060|10000:27999|Status' || ufw status

echo
echo "Готово. Если сервер за NAT — пробросьте на роутере:"
echo "  5060/udp, 5060/tcp, 10000-10005/udp → $(hostname -I | awk '{print $1}')"
echo
echo "TL-WR844N: диапазоны не поддерживаются — добавьте 8 правил Port Forwarding"
echo "  (см. examples/python-client/README-megafon-tplink.md)"
