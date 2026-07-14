#!/usr/bin/env bash
# Open ports for the Megafon Multifon SIP trunk on this host (ufw).
# Run: sudo bash examples/open-megafon-ports.sh

set -euo pipefail

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
  echo "Run with sudo: sudo bash $0" >&2
  exit 1
fi

echo "=== Current addresses ==="
ip -4 addr show scope global | grep -E 'inet ' || true
echo "Public IP (ifconfig.me): $(curl -s --max-time 5 ifconfig.me || echo '?')"
echo

echo "=== UFW: opening SIP + RTP (narrow pool for TL-WR844N) ==="
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
echo "Done. If the server is behind NAT, forward on the router:"
echo "  5060/udp, 5060/tcp, 10000-10005/udp → $(hostname -I | awk '{print $1}')"
echo
echo "TL-WR844N: port ranges are not supported — add 8 individual Port Forwarding rules"
echo "  (see examples/python-client/README-megafon-tplink.md)"
