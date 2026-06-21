# Megafon + TP-Link TL-WR844N

Роутер **TL-WR844N** в разделе **Advanced → NAT Forwarding → Port Forwarding**
принимает только **один порт** в External/Internal Port. Формат `10000-27999` не работает.

## Вариант A — Port Forwarding (8 правил)

Зарезервируйте IP ПК: **192.168.0.109**

**Advanced → NAT Forwarding → Port Forwarding → Add** — для каждой строки:

| External Port | Internal Port | Device IP     | Protocol |
|---------------|---------------|---------------|----------|
| 5060          | 5060          | 192.168.0.109 | UDP      |
| 5060          | 5060          | 192.168.0.109 | TCP      |
| 10000         | 10000         | 192.168.0.109 | UDP      |
| 10001         | 10001         | 192.168.0.109 | UDP      |
| …             | …             | …             | UDP      |
| 10013         | 10013         | 192.168.0.109 | UDP      |

Порты RTP **10000–10013** заданы в `examples/multifon.toml` (14 портов = 7 RTP-сессий).

Лимит роутера — **~16 правил**; не тратьте слоты на лишнее.

## Вариант B — DMZ (проще, менее безопасно)

**Advanced → NAT Forwarding → DMZ → Enable**

DMZ Host IP: **192.168.0.109**

Все входящие порты пойдут на ПК. Удобно для проверки; после теста лучше вернуться к варианту A.

## Проверка

```bash
source examples/multifon.env
python3 examples/python-client/sip_register.py --once
PYTHONUNBUFFERED=1 python3 examples/python-client/asr_bot.py --mode trunk
```

Звонок на номер из `SIP_NUMBER`.
