# Megafon + TP-Link TL-WR844N

The **TL-WR844N** router, under **Advanced → NAT Forwarding → Port Forwarding**,
accepts only **a single port** in External/Internal Port. The `10000-27999` range format does not work.

## Option A — Port Forwarding (8 rules)

Reserve the PC's IP: **192.168.0.109**

**Advanced → NAT Forwarding → Port Forwarding → Add** — for each row:

| External Port | Internal Port | Device IP     | Protocol |
|---------------|---------------|---------------|----------|
| 5060          | 5060          | 192.168.0.109 | UDP      |
| 5060          | 5060          | 192.168.0.109 | TCP      |
| 10000         | 10000         | 192.168.0.109 | UDP      |
| 10001         | 10001         | 192.168.0.109 | UDP      |
| …             | …             | …             | UDP      |
| 10013         | 10013         | 192.168.0.109 | UDP      |

RTP ports **10000–10013** are set in `examples/multifon.toml` (14 ports = 7 RTP sessions).

The router's limit is **~16 rules**; don't waste slots on anything extra.

## Option B — DMZ (simpler, less secure)

**Advanced → NAT Forwarding → DMZ → Enable**

DMZ Host IP: **192.168.0.109**

All inbound ports go to the PC. Handy for testing; after the test it's better to return to Option A.

## Verification

```bash
source examples/multifon.env
python3 examples/python-client/sip_register.py --once
PYTHONUNBUFFERED=1 python3 examples/python-client/asr_bot.py --mode trunk
```

Call the number in `SIP_NUMBER`.
