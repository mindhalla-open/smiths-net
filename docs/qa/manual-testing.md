# Ручное тестирование smiths-net

Документ описывает, как вручную проверить текущую реализацию движка
(v0.73.0). Покрытые подсистемы:

- **SIP-сигнализация**: UDP / TCP / TLS транспорты, UAS (`OPTIONS`,
  `INVITE`/`ACK`/`BYE`, `REGISTER`), digest-аутентификация
  (MD5 + SHA-256), per-IP rate limiting.
- **SDP/медиа**: offer/answer, rendezvous-мост, SSRC-rewrite,
  RTCP SR/RR, SRTP (SDES), транскодинг Opus ↔ G.711.
- **MCP**: stdio, HTTP (`POST /mcp`), SSE (`GET /mcp/events`),
  ресурсы (`health://status`, `sip://calls`, `sip://registrations`,
  `config://current`, `config://history`, `cluster://status`).
- **A2A**: HTTP JSON-RPC + agent card.
- **Плагины**: WASM (wasmtime) + sidecar (JSON-RPC subprocess),
  AI-инструменты (synthesize, transcribe, llm_chat, embed, speak),
  hot-reload, sandbox (rlimits, seccomp).
- **WebRTC**: WebSocket-сигнализация, DTLS-SRTP, ICE-Lite/Full,
  embedded TURN, tag-based rendezvous, SIP↔WebRTC bridge,
  privacy enforcement.
- **Управление**: `make_call` / `end_call`, `send_dtmf`,
  конференции, `get_config` / `put_config`, SIGHUP hot-reload
  с canary-таймером + error-rate probe.
- **Ops**: Prometheus `/metrics`, deep `/health`, graceful drain,
  HA (primary/secondary replication, Raft), CLI subcommands
  (`validate`, `reload`, `init`).
- **Storage**: CDR (SQLite), recording (filesystem), vector
  (in-memory / sidecar).

## Что уже должно работать

- Бинарник `smiths-net` стартует с TOML-конфигом.
- **SIP**: слушает UDP / TCP / TLS (настраиваемо).
- HTTP `/health` отвечает deep JSON на `127.0.0.1:8080`.
- HTTP `/metrics` отвечает OpenMetrics text.
- **UAS**:
  - `OPTIONS` → `200 OK`.
  - `INVITE` с SDP-offer → `100 Trying` → `200 OK` с SDP-answer.
  - Нет пересечения по кодекам → `488 Not Acceptable Here`.
  - `ACK` подтверждает диалог; `BYE` закрывает → `200 OK`.
  - `REGISTER` → `401` (challenge) → `200 OK` (auth), или `200 OK`
    без auth при `auth.backend = "none"`.
  - Неподдерживаемый метод → `405 Method Not Allowed`.
- **SRTP (SDES)**: `RTP/SAVP` + `a=crypto:` → шифрованный мост.
- **Rendezvous-мост**: два `INVITE`s с одинаковым ключом
  спариваются, RTP ходит A↔B. `BYE` гасит мост.
- **WebRTC**: WebSocket-сигнализация, DTLS-SRTP, tag-based
  rendezvous, ICE-Lite/Full, embedded TURN, SIP↔WebRTC bridge.
- **UAC**: `make_call` / `end_call` через MCP/A2A.
- **MCP**: stdio + HTTP (`POST /mcp`) + SSE (`GET /mcp/events`).
- **A2A**: HTTP JSON-RPC + agent card.
- **Плагины**: WASM (wasmtime) + sidecar (subprocess), AI-квартет
  (synthesize, transcribe, llm_chat, embed), speak, hot-reload.
- **Ops**: Prometheus, drain, SIGHUP hot-reload + canary + rollback,
  CLI подкоманды (`validate`, `reload`, `init`).
- **HA**: primary/secondary replication, Raft consensus.
- **Storage**: CDR (SQLite), recording (fs), vector (memory).
- Ретрансмиссия по UDP → кэшированный ответ.
- Per-IP SIP rate limiting.
- На `SIGINT`/`SIGTERM` — graceful drain + shutdown.

---

## 0. Подготовка окружения

### 0.1 Требуется

- Rust toolchain (см. `rust-toolchain.toml`).
- `curl` — для проверки health.
- Любое из: `sipp`, `pjsua`, `nc`/`ncat`, `socat` — для отправки
  SIP-пакетов.
- Для JSON-логов удобнее иметь `jq`.

### 0.2 Сборка

```bash
cargo build -p smiths-cli
```

Бинарник окажется в `target/debug/smiths-net`.

### 0.3 Конфиг

Пример лежит в `examples/config.toml`. Для локальных тестов можно
использовать его как есть либо скопировать:

```bash
cp examples/config.toml /tmp/smiths.toml
```

Ключевые параметры:

| Ключ | По умолчанию | Что делает |
|---|---|---|
| `sip.bind` | `["0.0.0.0:5060"]` | адреса SIP-сокетов |
| `sip.transports` | `["udp"]` | `udp`, `tcp`, `tls` (+ `quic` scaffold) |
| `sip.tls_cert_path` | — | PEM-сертификат для TLS |
| `sip.tls_key_path` | — | PEM-ключ для TLS |
| `sip.rate_limit.per_sec` | `0` (выкл.) | лимит пакетов/с на IP |
| `observability.health_bind` | `127.0.0.1:8080` | HTTP `/health` + `/metrics` |
| `observability.log_level` | `info` | уровень логов |
| `observability.log_format` | `json` | `json` или `pretty` |
| `auth.backend` | `none` | `none` / `sqlite` / `http` |
| `auth.realm` | `smiths.local` | realm для digest-auth |
| `storage.backend` | `none` | `none` / `sqlite` (CDR + KV) |
| `storage.vector.backend` | `none` | `none` / `memory` / `sidecar` |
| `storage.recording.backend` | `none` | `none` / `fs` / `sidecar` |
| `media.inband_dtmf` | `false` | Goertzel DTMF-детектор |
| `media.transcode.max_concurrent_calls` | `40` | бюджет транскодинга |
| `plugins.dir` | `plugins` | корень загрузки плагинов |
| `mcp.enabled_http` | `false` | HTTP-транспорт MCP |
| `mcp.http_bind` | `127.0.0.1:7880` | bind для MCP HTTP |
| `a2a.enabled` | `false` | A2A HTTP-адаптер |
| `a2a.bind` | `127.0.0.1:7879` | bind для A2A |
| `webrtc.enabled` | `false` | WebRTC-адаптер |
| `webrtc.ws_bind` | `127.0.0.1:7881` | WebSocket bind |
| `cluster.mode` | `standalone` | `standalone` / `primary` / `secondary` |

Переменные окружения с префиксом `SMITHS__` переопределяют конфиг,
например: `SMITHS__OBSERVABILITY__LOG_LEVEL=debug`.

> ⚠️ Порт 5060 на Linux часто требует CAP_NET_BIND_SERVICE только для
> портов < 1024 как root; 5060 не входит, но может быть занят другим
> SIP-софтом (например, `pjsua`, Asterisk). Если порт занят —
> поменяйте `sip.bind` на `127.0.0.1:5070` (или другой свободный).

---

## 1. Smoke-тест: запуск и завершение

### 1.1 Запуск с pretty-логами

```bash
SMITHS__OBSERVABILITY__LOG_FORMAT=pretty \
  target/debug/smiths-net --config examples/config.toml
```

Ожидаемый вывод (по порядку):

- `smiths-net starting`
- `health endpoint listening`
- `SIP UDP listening` с указанием `local=0.0.0.0:5060`
- `smiths-net ready`

### 1.2 Graceful shutdown

В другом терминале:

```bash
pkill -INT smiths-net   # или Ctrl+C в окне, где он запущен
```

Ожидаемо:

- `shutdown signal received; draining`
- `health endpoint stopped`
- `graceful shutdown complete`
- Процесс вышел с кодом `0`.

### 1.3 Проверка занятого порта

Запустите две копии подряд. Вторая **не должна падать** — она должна
только вывести `warn`:
`failed to start SIP on bind; continuing` с причиной
`Address already in use`. Это поведение «продолжаем, даже если один
бинд не поднялся».

---

## 2. Health-эндпойнт

При запущенном движке:

```bash
curl -sS http://127.0.0.1:8080/health | jq .
# → {
#     "status": "ok",
#     "draining": false,
#     "uptime_secs": 5,
#     "sip": { "binds": ["udp://0.0.0.0:5060"] },
#     "plugins": { "loaded": [...], "failed": [] },
#     "dialogs_active": 0,
#     "bridges_active": 0
#   }
curl -o /dev/null -s -w "%{http_code}\n" http://127.0.0.1:8080/health
# → 200
```

Любой путь, кроме `/health` и `/metrics`, должен вернуть `404`:

```bash
curl -o /dev/null -s -w "%{http_code}\n" http://127.0.0.1:8080/foo
# → 404
```

---

## 3. SIP `OPTIONS` → `200 OK`

### 3.1 Способ A: `sipp`

Создайте файл `/tmp/options.xml`:

```xml
<?xml version="1.0" encoding="ISO-8859-1" ?>
<scenario name="OPTIONS ping">
  <send>
    <![CDATA[
      OPTIONS sip:ping@[remote_ip]:[remote_port] SIP/2.0
      Via: SIP/2.0/UDP [local_ip]:[local_port];branch=[branch]
      From: sipp <sip:sipp@[local_ip]>;tag=[call_number]
      To: ping <sip:ping@[remote_ip]>
      Call-ID: [call_id]
      CSeq: 1 OPTIONS
      Max-Forwards: 70
      User-Agent: SIPp
      Content-Length: 0

    ]]>
  </send>

  <recv response="200" rtd="true"/>
</scenario>
```

Запуск (на отдельной машине или на localhost):

```bash
sipp -sf /tmp/options.xml -m 1 127.0.0.1:5060
```

Критерий успеха: в отчёте `sipp` — `Successful call` = 1, сценарий
завершается с кодом `0`.

### 3.2 Способ B: сырой UDP через `nc`

Собираем валидный пакет (строки должны разделяться `\r\n`):

```bash
printf 'OPTIONS sip:ping@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:6060;branch=z9hG4bK-test-1;rport\r\nFrom: test <sip:test@127.0.0.1>;tag=abc\r\nTo: ping <sip:ping@127.0.0.1>\r\nCall-ID: manual-qa-1@localhost\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n' \
  | nc -u -w1 127.0.0.1 5060
```

Ожидаемо:

- В stdout: строка `SIP/2.0 200 OK` + скопированные `Via`, `From`,
  `Call-ID`, `CSeq`, заголовок `To` с добавленным `;tag=smiths-...`,
  `Content-Length: 0`.
- В логах движка:
  - `RequestReceived` (метод `OPTIONS`),
  - `ResponseSent` со `status=200`.

### 3.3 Способ C: `pjsua`

```bash
pjsua --null-audio --no-tcp \
  --local-port=6070 \
  sip:127.0.0.1:5060
# в REPL: im
# затем команда: `O` (send OPTIONS) на URI sip:ping@127.0.0.1
```

`pjsua` должен показать `200 OK`.

---

## 4. Неподдерживаемые методы → `405`

```bash
printf 'MESSAGE sip:127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:6060;branch=z9hG4bK-msg-1\r\nFrom: test <sip:test@127.0.0.1>;tag=msg1\r\nTo: test <sip:test@127.0.0.1>\r\nCall-ID: manual-qa-msg@localhost\r\nCSeq: 1 MESSAGE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n' \
  | nc -u -w1 127.0.0.1 5060
```

Ожидаемо: `SIP/2.0 405 Method Not Allowed`. Повторить для
`SUBSCRIBE`, `NOTIFY`, `PUBLISH` — все должны получить `405`.

> `REGISTER` теперь поддерживается — см. раздел 16.
> `INVITE` — см. раздел 10.

---

## 5. Дедупликация ретрансмиссий

UDP ненадёжен, и SIP-стеки повторяют запросы с тем же
`Via: ...;branch=...`. Движок обязан вернуть **тот же** кэшированный
ответ, а не собирать новый (новый `To;tag` нарушил бы транзакцию).

Шаги:

1. Отправьте пакет из п. 3.2 дважды подряд:
   ```bash
   for i in 1 2; do
     printf 'OPTIONS sip:ping@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:6060;branch=z9hG4bK-dup-1;rport\r\nFrom: t <sip:t@127.0.0.1>;tag=dup\r\nTo: p <sip:p@127.0.0.1>\r\nCall-ID: dup-qa@localhost\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n' \
       | nc -u -w1 127.0.0.1 5060
     echo '---'
   done
   ```
2. Оба ответа должны быть **побайтово идентичны**, особенно `To;tag=`.
3. В логах с `--log debug` второй запрос должен сопровождаться записью
   `replaying cached response`.

Если `branch` поменять — появится новый `To;tag=`.

---

## 6. Некорректные пакеты

Движок не должен падать/зависать на мусоре.

```bash
# случайные байты
head -c 200 /dev/urandom | nc -u -w1 127.0.0.1 5060

# обрезанный запрос
printf 'OPTIONS sip:x SIP/2.0\r\nVia: SIP/2.0/UDP\r\n' | nc -u -w1 127.0.0.1 5060

# пустая датаграмма
printf '' | nc -u -w1 127.0.0.1 5060
```

Ожидаемо:

- Клиент ответа не получает (тайм-аут `nc -w1`).
- В логах: `warn` с текстом `malformed SIP message dropped` + `reason`.
- Процесс жив, `/health` по-прежнему отвечает.

---

## 7. Параллельная нагрузка (smoke)

Не нагрузочный тест, а проверка, что нет гонок при >1 запроса.

```bash
for i in $(seq 1 50); do
  (printf "OPTIONS sip:ping@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:6060;branch=z9hG4bK-load-$i;rport\r\nFrom: t <sip:t@127.0.0.1>;tag=l$i\r\nTo: p <sip:p@127.0.0.1>\r\nCall-ID: load-$i@localhost\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n" \
    | nc -u -w1 127.0.0.1 5060 > /tmp/smiths-load-$i.txt) &
done
wait
grep -c '200 OK' /tmp/smiths-load-*.txt | grep -c ':1$'
# → 50
```

Все 50 ответов — `200 OK`. Движок не паникует, `/health` живой.

---

## 8. Проверка логов

### 8.1 JSON-логи

С `log_format = "json"` каждая строка — валидный JSON:

```bash
SMITHS__OBSERVABILITY__LOG_FORMAT=json \
  target/debug/smiths-net 2>&1 | jq -c '.'
```

Ни одной строки `parse error` — значит формат валиден.

### 8.2 Уровень логов

```bash
RUST_LOG=smiths_sip=debug,info target/debug/smiths-net
```

Ожидаемо: в stdout появляются `debug`-записи из `smiths-sip`
(например, `replaying cached response`), но не `trace`.

---

## 9. Конфиг: отключение UDP

В `sip.transports` оставьте пустой список либо `["tcp"]`:

```toml
[sip]
bind = ["0.0.0.0:5060"]
transports = []
```

Ожидаемо при запуске:

- `warn`: `no UDP SIP transport configured; signaling disabled`.
- Порт 5060 **не** открывается (проверяйте через `lsof -iUDP:5060`).
- `/health` продолжает работать.

---

## 10. INVITE / ACK / BYE с SDP

Проверка базового диалога: движок принимает `INVITE` с SDP-offer,
отвечает `100 Trying` + `200 OK` со своим SDP-answer и выделенным
RTP-портом, `ACK` переводит диалог в Confirmed, `BYE` закрывает его.

### 10.1 Автоматический тест

```bash
cargo test -p smiths-sip --test invite
cargo test -p smiths-sip --test sdp
```

Должны пройти:

- `invite_establishes_dialog_ack_then_bye`
- `bye_without_dialog_returns_481`
- `invite_retransmit_replays_same_200`
- `invite_with_sdp_offer_gets_sdp_answer`
- `invite_with_only_unknown_codecs_returns_488`

### 10.2 Ручной прогон через `pjsua`

```bash
# В одном терминале запускаем движок:
SMITHS__OBSERVABILITY__LOG_FORMAT=pretty \
  target/debug/smiths-net --config examples/config.toml

# В другом — звоним:
pjsua --null-audio --local-port=6070 sip:anything@127.0.0.1:5060
# В REPL pjsua:  m  (make call)  →  введите URI sip:anything@127.0.0.1
```

Ожидаемо:

- `pjsua` показывает `CALL CONFIRMED`.
- В логах движка:
  - `RequestReceived method=INVITE`
  - `ResponseSent status=100`, затем `status=200`.
  - `dialog confirmed` после прихода `ACK` (уровень `info`).
- На `h` (hangup) в `pjsua` → логи покажут `DialogTerminated`.

### 10.3 488 при несовместимых кодеках

В `pjsua` можно форсировать только неподдерживаемый кодек:

```bash
pjsua --null-audio --add-codec=G722 --dis-codec=PCMU --dis-codec=PCMA \
  sip:x@127.0.0.1:5060
# затем `m` → sip:x@127.0.0.1
```

Движок должен ответить `488 Not Acceptable Here` (нет пересечения с
PCMU/PCMA/Opus). `pjsua` покажет `CALL DISCONNECTED … 488`.

---

## 11. Two-UA rendezvous bridge (аудио end-to-end)

Самый большой e2e: два тестовых UA в одной локалхост-сети звонят через
движок по одному и тому же `sip:<key>@engine`, движок их спаривает и
пересылает RTP байт-в-байт. Подкидываем сгенерированный синус, второй
UA записывает принятое в WAV — на выходе получаем тот же сигнал,
который можно прослушать.

### 11.1 Запуск автоматического теста

```bash
cargo test -p smiths-testkit --test audio_bridge
```

Что он делает (см. `crates/smiths-testkit/tests/audio_bridge.rs`):

1. Поднимает движок на loopback с эфемерным портом.
2. Создаёт два `TestUac` (каждый бинарит свой UDP-сокет под SIP и под RTP).
3. Параллельно шлёт два `INVITE sip:call-1@engine` — второй `INVITE`
   триггерит мост.
4. UA-A генерирует 1 кГц синус (1 секунда, 8 кГц PCM16), кодирует в
   μ-law, пакует в 50 RTP-пакетов по 20 мс и шлёт на выделенный
   движком порт.
5. UA-B слушает свой RTP-сокет, собирает пакеты, проверяет, что
   «средний хвост» принятого μ-law **побайтово** совпадает с тем, что
   UA-A отправил.
6. Принятое декодируется μ-law → PCM16 и пишется в
   `/tmp/smiths-call-received.wav`.
7. Оба UA делают `BYE`, мост гасится.

Успех теста — `test result: ok. 1 passed`.

### 11.2 Прослушивание результата

После успешного теста в `/tmp/smiths-call-received.wav` лежит стандартный
RIFF/WAVE PCM16 моно 8 кГц — ровно 1 секунда.

```bash
file /tmp/smiths-call-received.wav
# → RIFF (little-endian) data, WAVE audio, Microsoft PCM, 16 bit, mono 8000 Hz

# macOS:
afplay /tmp/smiths-call-received.wav

# Linux (любое):
aplay /tmp/smiths-call-received.wav
# или:
ffplay -autoexit -nodisp /tmp/smiths-call-received.wav
```

Должен звучать чистый 1 кГц тон 1 секунду. Если слышны щелчки, провалы
или тишина — значит мост где-то теряет пакеты.

### 11.3 Ручной вариант через два `pjsua`

Если хочется «живого» звука через движок (без тестового UAC):

```bash
# Терминал 1 — движок:
target/debug/smiths-net --config examples/config.toml

# Терминал 2 — UA-A (играет WAV в линию):
pjsua --play-file=~/Music/sample.wav --auto-play \
  --local-port=6070 --no-tcp \
  --add-codec=PCMU --dis-codec=PCMA --dis-codec=opus \
  sip:room-42@127.0.0.1:5060
# в REPL: m  → sip:room-42@127.0.0.1

# Терминал 3 — UA-B (пишет принятое в WAV):
pjsua --rec-file=/tmp/received.wav --auto-rec \
  --local-port=6071 --no-tcp \
  --add-codec=PCMU --dis-codec=PCMA --dis-codec=opus \
  sip:room-42@127.0.0.1:5060
# в REPL: m  → sip:room-42@127.0.0.1
```

Оба используют один и тот же rendezvous-ключ `room-42` — движок их
свяжет. После `h` (hangup) в обоих pjsua откройте `/tmp/received.wav` —
должен быть записан звук из `sample.wav`.

> ⚠️ `pjsua` без `--null-audio` захватывает системный микрофон; для
> теста с файлами это не нужно — `--play-file`/`--rec-file` достаточно.

### 11.4 Что ищем в логах движка

При успешном мосте (с `--log debug`):

- `RequestReceived method=INVITE` × 2.
- `rendezvous leg parked, awaiting peer` — после первого `INVITE`.
- `rendezvous bridge established` — после второго.
- `DialogCreated` × 2.
- После `BYE` с любой стороны: `rendezvous bridge stopped`,
  `DialogTerminated`.

---

## 12. Control plane: MCP (stdio) — ручная проверка

С **v0.3.0** движок умеет работать как MCP-сервер по stdio. Это тот
самый wire, который используют LLM-хосты (Claude Code, Cursor и т.п.),
запуская движок как дочерний процесс.

### 12.1 Автоматические тесты

```bash
cargo test -p smiths-mcp
```

Ожидаемо: `mcp::tests::*` + `tools::tests::*` + `control::tests::*` —
все зелёные.

### 12.2 Самый быстрый ручной прогон — встроенный Python-демо

```bash
cargo build --release
python3 examples/python-client/mcp_demo.py
```

Скрипт **сам** стартует движок с `--mcp stdio` и прогоняет:

1. `initialize` → ожидаем `serverInfo.name == "smiths-net"`,
   `protocolVersion == "2024-11-05"`.
2. `notifications/initialized` → ответа не ждём (это нотификация).
3. `tools/list` → должны быть все зарегистрированные инструменты (включая
   `list_calls`, `get_call_status`, `health`, `make_call`, `end_call`,
   `speak`, `send_dtmf`, `synthesize`, `transcribe`, `llm_chat`, `embed`,
   `list_ai_providers`, `describe_provider`, `list_cdr`, `list_metrics`,
   `get_metric`, `reload_plugin`, `get_config`, `put_config`,
   `record_prompt`, `put_script`, `search_calls_semantic`,
   `transcribe_call`, `summarize_call`, `translate`,
   `create_conference`, `join_conference`, `leave_conference`),
   у каждого — валидная JSON Schema.
4. `tools/call` → `health` → `isError: false`, `uptime_secs`,
   `live_calls`.
5. `tools/call` → `get_call_status` с несуществующим id →
   `isError: true` + сообщение `not found: call does-not-exist`.

В конце в stderr виден лог движка (`"MCP stdio server ready"` /
`"MCP stdio server stopped"`).

### 12.3 Сырая проверка вручную — через `jq` + pipe

Хочется увидеть именно тот JSON-RPC, который идёт по wire — без
обёрток:

```bash
cargo build --release
(
  # initialize
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}'
  # initialized (notification — ответа не будет)
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  # tools/list
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  # health
  echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"health","arguments":{}}}'
  # неизвестный метод → ожидаем JSON-RPC error -32601
  echo '{"jsonrpc":"2.0","id":4,"method":"does/not/exist"}'
) | ./target/release/smiths-net --config examples/config.toml --mcp stdio 2>/tmp/mcp-stderr.log | jq '.'

echo '--- stderr (лог движка) ---'
cat /tmp/mcp-stderr.log
```

Ожидаемо в stdout (одна строка на ответ — `jq` отформатирует):

- id=1 → `result.serverInfo.name` = `"smiths-net"`.
- id=2 → `result.tools` — массив из трёх инструментов.
- id=3 → `result.structuredContent.status == "ok"`.
- id=4 → `error.code == -32601`, message начинается с `unknown method`.

В stderr видны **только** tracing-логи движка — ни одной «залётной»
JSON-RPC строки в stderr быть не должно (это гарантируется режимом
`--mcp stdio`: вся диагностика идёт в stderr, stdout остаётся чистым
wire'ом).

### 12.4 Проверка видимости live-звонков через MCP

Показывает, что `ControlState` действительно подписан на event-bus.

```bash
# Терминал 1 — движок (SIP ON, MCP over HTTP пока нет, так что сам
# MCP подключим тем же pipe-трюком). Нужен запущенный движок с SIP,
# чтобы было откуда появиться диалогу.
./target/release/smiths-net --config examples/config.toml &
ENGINE=$!

# Терминал 2 — тестовый прозвон из python-client
python3 examples/python-client/demo_call.py --wav tmp/smiths-hello.wav &
CALL=$!
sleep 1    # даём паре INVITE долететь до движка

# Терминал 3 — ничего, MCP работает только в режиме `--mcp stdio`;
# для live-проверки удобнее А2А (см. раздел 13). Останавливаем:
kill $CALL $ENGINE 2>/dev/null
```

> ℹ️ Если хотите видеть live-звонки через MCP — на сегодня это
> требует HTTP-транспорта для MCP (в планах), либо параллельного
> запуска A2A. См. раздел 13.

### 12.5 Интеграция с Claude Code

В `~/.config/claude-code/mcp.json` (или эквиваленте в Claude Desktop):

```jsonc
{
  "mcpServers": {
    "smiths-net": {
      "command": "/абсолютный/путь/target/release/smiths-net",
      "args": ["--config", "/абсолютный/путь/examples/config.toml",
               "--mcp", "stdio"]
    }
  }
}
```

Перезапустите Claude Code. В чате можно попросить:
«Покажи текущие звонки через smiths-net», — Claude вызовет
`list_calls` и отобразит результат. `health` работает так же.

### 12.6 MCP Inspector (опционально)

Официальный референсный UI от Anthropic:

```bash
npx @modelcontextprotocol/inspector \
  ./target/release/smiths-net --config examples/config.toml --mcp stdio
```

В браузере открывается UI, где видны все зарегистрированные
инструменты, их схемы, и можно вызвать каждый из формы.

### 12.7 MCP по HTTP (`POST /mcp`)

Включить в конфиге:

```toml
[mcp]
enabled_http = true
http_bind    = "127.0.0.1:7880"
```

```bash
./target/release/smiths-net --config examples/config.toml
```

В логах: `MCP HTTP server listening addr=127.0.0.1:7880`.

```bash
# tools/list
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
# → 28 (количество может расти)

# tools/call health
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"health","arguments":{}}}' \
  | jq '.result.structuredContent'
```

### 12.8 MCP SSE (`GET /mcp/events`)

При включённом `[mcp] enabled_http`:

```bash
# Терминал 1 — подписка на события
curl -N http://127.0.0.1:7880/mcp/events

# Терминал 2 — прозвон через pjsua
pjsua --null-audio --local-port=6070 sip:test@127.0.0.1:5060
```

Ожидаемо: в Терминале 1 появятся SSE-события `call/created`,
после hangup — `call/terminated`. Keep-alive ping каждые 15 с.

### 12.9 MCP-ресурсы

Через любой MCP-транспорт (ниже пример через HTTP):

```bash
# Список ресурсов
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"resources/list"}' | jq '.result.resources[].uri'
# → "health://status"
# → "sip://calls"
# → "sip://registrations"
# → "config://current"
# → "config://history"
# → "cluster://status"

# Чтение ресурса
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"config://current"}}' \
  | jq '.result.contents[0].text' | jq .
```

Ожидаемо: в `config://current` секреты (`openai_api_key`, `password`)
заменены на `"***"`.

### 12.10 MCP-инструменты `get_config` / `put_config`

```bash
# Получить текущий конфиг
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_config","arguments":{}}}' \
  | jq '.result'

# Изменить log_level на лету
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"put_config","arguments":{"patch":{"observability":{"log_level":"debug"}}}}}' \
  | jq '.result'
```

Ожидаемо: в логах движка — `log filter reloaded`,
уровень `debug` начинает выводиться.

---

## 13. Control plane: A2A (HTTP) — ручная проверка

A2A (agent-to-agent) адаптер поднимает JSON-RPC поверх HTTP на
отдельном порту. Одно и то же множество инструментов, что и у MCP, но
вызываемое извне без запуска движка как subprocess.

### 13.1 Включить A2A в конфиге

```bash
cat > tmp/a2a.toml <<'EOF'
[observability]
log_format  = "pretty"
health_bind = "127.0.0.1:8080"

[sip]
bind       = ["127.0.0.1:5060"]
transports = ["udp"]

[a2a]
enabled = true
bind    = "127.0.0.1:7879"
EOF

./target/release/smiths-net --config tmp/a2a.toml
```

В логах запуска должны увидеть строку
`A2A HTTP server listening addr=127.0.0.1:7879`, а в
`smiths-net ready` — поле `a2a_enabled=true`.

### 13.2 Встроенный Python-демо

```bash
python3 examples/python-client/a2a_demo.py --url http://127.0.0.1:7879
```

Скрипт должен:

1. Получить agent card с `GET /.well-known/agent.json` — в ответе
   `name: "smiths-net"`, массив `capabilities.tools`.
2. `POST /a2a` с `tools/list` → те же три инструмента.
3. `tools/call health` → `result.output.status == "ok"`.
4. `tools/call list_calls` → `count: 0` (пока никто не звонит).
5. `tools/call get_call_status` с несуществующим id → поле `error` с
   кодом `-32001` (`tool-not-found` в нашей app-range).

### 13.3 Сырой `curl` для протокольной проверки

Agent card:

```bash
curl -s http://127.0.0.1:7879/.well-known/agent.json | jq '.name, .endpoint, [.capabilities.tools[].name]'
# "smiths-net"
# "/a2a"
# [список всех зарегистрированных инструментов]
```

`tools/list`:

```bash
curl -s -X POST http://127.0.0.1:7879/a2a \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools[].name'
# "get_call_status"
# "health"
# "list_calls"
```

`tools/call health`:

```bash
curl -s -X POST http://127.0.0.1:7879/a2a \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"health","arguments":{}}}' | jq '.result.output'
# {
#   "status": "ok",
#   "uptime_secs": 12,
#   "live_calls": 0,
#   "known_calls": 0
# }
```

### 13.4 Live-проверка счётчика звонков

A2A работает одновременно с SIP, поэтому через него удобно видеть
счётчик `live_calls` прямо во время прозвона:

```bash
# Терминал 1 — движок из п.13.1
./target/release/smiths-net --config tmp/a2a.toml

# Терминал 2 — следим за health раз в секунду
while sleep 1; do
  curl -s -X POST http://127.0.0.1:7879/a2a \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"health"}}' \
    | jq -c '.result.output | {uptime_secs, live_calls, known_calls}'
done

# Терминал 3 — запускаем прозвон
python3 examples/python-client/demo_call.py --wav tmp/smiths-hello.wav
```

Ожидаемо: `live_calls` на пару секунд прыгает в `2`, затем обратно в
`0`; `known_calls` инкрементируется на 2 (оба диалога оседают в
registry в статусе `terminated`).

### 13.5 Ошибочные пути

| Запрос | Ожидаемый код | Поле |
|---|---|---|
| `{"method":"unknown"}` | `-32601` | `error.code` = method-not-found |
| `{"method":"tools/call","params":{"name":"nope"}}` | `-32601` | unknown tool |
| `{"method":"tools/call","params":{"name":"get_call_status","arguments":{}}}` | `-32602` | missing required arg |
| `POST /a2a` с битым JSON | HTTP 400 от axum | — |

---

## 14. TCP-транспорт

```bash
cat > /tmp/smiths-tcp.toml <<'EOF'
[observability]
log_format  = "pretty"
health_bind = "127.0.0.1:8080"

[sip]
bind       = ["127.0.0.1:5060"]
transports = ["udp", "tcp"]
EOF

./target/release/smiths-net --config /tmp/smiths-tcp.toml
```

В логах: `SIP TCP listening local=127.0.0.1:5060`.

### 14.1 Автоматический тест

```bash
cargo test -p smiths-sip --test tcp_loss_bench
```

### 14.2 Ручная проверка через pjsua

```bash
pjsua --null-audio --local-port=6070 --use-tcp \
  sip:anything@127.0.0.1:5060
```

Ожидаемо: `CALL CONFIRMED` через TCP.

---

## 15. TLS-транспорт

Требуется PEM-сертификат и ключ. Для тестов — self-signed:

```bash
cargo test -p smiths-sip --test tls
```

Ручной прогон:

```toml
[sip]
bind          = ["127.0.0.1:5061"]
transports    = ["tls"]
tls_cert_path = "/tmp/smiths-cert.pem"
tls_key_path  = "/tmp/smiths-key.pem"
```

В логах: `SIP TLS listening local=127.0.0.1:5061`.

---

## 16. REGISTER + digest-аутентификация

### 16.1 Dev-режим (без auth backend)

С `auth.backend = "none"` (по умолчанию) движок принимает REGISTER
без challenge — ответ `200 OK` сразу.

### 16.2 С SQLite-бэкендом

```toml
[auth]
backend = "sqlite"
realm   = "smiths.local"

[auth.sqlite]
path = "/tmp/smiths-auth.db"
```

Движок отвечает `401 Unauthorized` с `WWW-Authenticate` (digest,
nonce, realm). Клиент повторяет с `Authorization`.

### 16.3 Автоматические тесты

```bash
cargo test -p smiths-sip --test register
cargo test -p smiths-sip --test register_sqlite
cargo test -p smiths-sip --test register_http
cargo test -p smiths-sip --test invite_auth
```

### 16.4 sipp-сценарий

```bash
SMITHS_TEST_CREDS=alice:smiths.local:secret \
  ./target/release/smiths-net --config examples/config.toml &
sipp -sf scenarios/sipp/register.xml -m 100 127.0.0.1:5060
```

Критерий: 100 Successful calls, код 0.

### 16.5 С HTTP-webhook бэкендом

```toml
[auth]
backend = "http"

[auth.http]
endpoint   = "https://iam.example.com/sip-auth"
timeout_ms = 2000
```

Автотест: `cargo test -p smiths-sip --test register_http`.

---

## 17. SIP rate limiting

```toml
[sip.rate_limit]
per_sec = 10
burst   = 20
```

```bash
# Отправить 50 пакетов быстрее лимита
for i in $(seq 1 50); do
  (printf "OPTIONS sip:ping@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:6060;branch=z9hG4bK-rl-$i;rport\r\nFrom: t <sip:t@127.0.0.1>;tag=rl$i\r\nTo: p <sip:p@127.0.0.1>\r\nCall-ID: rl-$i@localhost\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n" \
    | nc -u -w1 127.0.0.1 5060 > /tmp/smiths-rl-$i.txt) &
done
wait
grep -c '200 OK' /tmp/smiths-rl-*.txt | grep -c ':1$'
# → менее 50 (часть отброшена лимитером)
```

Автотест: `cargo test -p smiths-sip --test rate_limit`.

---

## 18. SRTP (SDES)

### 18.1 Автоматические тесты

```bash
cargo test -p smiths-sip --test sdp_srtp
cargo test -p smiths-sip --test codec_mismatch
```

Ключевые сценарии:

- Два UA с `RTP/SAVP` + `a=crypto:` AES_CM_128_HMAC_SHA1_80 →
  мост с SRTP-шифрованием, побайтовый round-trip.
- `RTP/SAVP` без `a=crypto:` → `488 Not Acceptable Here`.
- Неподдерживаемый crypto-suite → `488`.

---

## 19. Prometheus-метрики

При запущенном движке:

```bash
curl -sS http://127.0.0.1:8080/metrics | head -30
```

Ожидаемо: валидный OpenMetrics text. Ключевые семейства:

- `smiths_sip_requests_total{method}` — счётчик по методам.
- `smiths_sip_responses_total{code}` — счётчик по кодам.
- `smiths_sip_dialogs_active` — gauge активных диалогов.
- `smiths_tool_invocations_total{tool,outcome}` — MCP-вызовы.
- `smiths_tool_duration_seconds{tool}` — гистограмма задержек.
- `smiths_bridges_active` — gauge активных медиа-мостов.
- `smiths_rtp_packets_forwarded` — счётчик переданных RTP.
- `smiths_rtcp_sr_sent` — отправленных RTCP SR.
- `smiths_plugin_invocations{plugin}` — по плагинам.

---

## 20. Плагины (WASM + sidecar)

### 20.1 WASM-плагины

```bash
cargo test -p smiths-wasm
```

Тесты покрывают: host-log, trap isolation, fuel exhaustion,
missing export, epoch-interruption timeout, state_get/set,
publish_event, timer_set, send_rtp.

### 20.2 Sidecar-плагины

```bash
cargo test -p smiths-plugin
cargo test -p smiths-sidecar
```

Тесты покрывают: describe_capabilities, invoke round-trip,
control validation, restart policy + exponential backoff,
concurrent RPCs (32 in-flight), plugin → engine notifications
(streaming), hot-reload file watcher.

### 20.3 Проверка загрузки

```bash
ls plugins/examples/
# ai-tts-mock  ai-asr-mock  ai-llm-mock  ai-embed-mock  rust-logger  ...

SMITHS__PLUGINS__DIR=plugins/examples \
  ./target/release/smiths-net --config examples/config.toml
```

В логах: `plugins ready loaded=[...]`.

---

## 21. AI-инструменты

При загруженных mock-плагинах (п. 20.3):

```bash
# Список провайдеров
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_ai_providers","arguments":{}}}' \
  | jq '.result'

# Синтез речи
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"synthesize","arguments":{"plugin":"ai-tts-mock","text":"Hello world"}}}' \
  | jq '.result'

# LLM-чат
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"llm_chat","arguments":{"plugin":"ai-llm-mock","messages":[{"role":"user","content":"hello"}]}}}' \
  | jq '.result'
```

---

## 22. Исходящие звонки (UAC)

```bash
# Через MCP HTTP (нужен запущенный UAS на той стороне)
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"make_call","arguments":{"target":"sip:echo@127.0.0.1:6070"}}}' \
  | jq '.result'

# Завершить звонок
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"end_call","arguments":{"call_id":"<id из make_call>"}}}' \
  | jq '.result'
```

Автотест: `cargo test -p smiths-sip --test uac`.

---

## 23. WebRTC

### 23.1 Конфиг

```toml
[webrtc]
enabled = true
ws_bind = "127.0.0.1:7881"
```

В логах: `WebRTC WebSocket listening addr=127.0.0.1:7881`.

### 23.2 Browser-демо

Открыть `examples/browser-webrtc/index.html` (или подать через
`python3 -m http.server`). WebSocket подключается к
`ws://127.0.0.1:7881/smiths/webrtc`.

### 23.3 DTLS-SRTP

Автотесты:

```bash
cargo test -p smiths-media --test dtls_handshake
cargo test -p smiths-sdp -- dtls
```

### 23.4 Tag-based rendezvous

Два WebRTC-клиента с одинаковым `tag` в `session-init` спариваются;
движок устанавливает мост. BYE с любой стороны освобождает мост.

### 23.5 SIP↔WebRTC bridge

SIP INVITE с заголовком `X-Smiths-Webrtc-Tag: <tag>` присоединяется
к WebRTC-стороне rendezvous. Требуется `[webrtc] enabled = true` +
UAS с `with_webrtc_rendezvous`.

### 23.6 ICE + TURN

```toml
[webrtc.ice]
enabled = true

[webrtc.turn]
enabled = true
bind    = "0.0.0.0:3478"
realm   = "turn.example.com"
relay_ip = "127.0.0.1"
credentials = [{ username = "alice", password = "hunter2" }]
```

Автотест: `cargo test -p smiths-ice --test turn_allocation`.

---

## 24. Config hot-reload (SIGHUP)

```bash
# Терминал 1 — движок
./target/release/smiths-net --config examples/config.toml &
ENGINE=$!

# Терминал 2 — поменять log_level в конфиге
sed -i 's/log_level = "info"/log_level = "debug"/' examples/config.toml
kill -HUP $ENGINE
```

Ожидаемо в логах:

- `SIGHUP received; reloading config`
- `SIGHUP reload: canary window armed` с `reloaded = ["observability.log_level"]`
- `log filter reloaded new_level=debug`

Поля, отмеченные `#[restart_required]` (например, `sip.bind`),
генерируют `warn` и не применяются до рестарта.

Автотесты: `cargo test -p smiths-cli --test config_reload`.

---

## 25. CLI-подкоманды

### 25.1 `validate`

```bash
./target/release/smiths-net --config examples/config.toml validate
echo $?
# → 0 (конфиг валиден)

./target/release/smiths-net --config /dev/null validate
echo $?
# → 1 (parse error)
```

### 25.2 `reload`

```bash
./target/release/smiths-net --config examples/config.toml reload \
  --pid $ENGINE --diff --dry-run
```

С `--dry-run` — показывает diff, не отправляет SIGHUP.

### 25.3 `init`

```bash
./target/release/smiths-net init --preset dev --non-interactive --output /tmp/dev.toml
./target/release/smiths-net --config /tmp/dev.toml validate
# → 0

./target/release/smiths-net init --preset prod --non-interactive --output /tmp/prod.toml
./target/release/smiths-net --config /tmp/prod.toml validate
# → 0
```

Автотесты: `cargo test -p smiths-cli --test init_test`.

---

## 26. Graceful drain

```bash
# Терминал 1 — движок
./target/release/smiths-net --config examples/config.toml

# Терминал 2 — начать звонок
pjsua --null-audio --local-port=6070 sip:room@127.0.0.1:5060
# → m → sip:room@127.0.0.1

# Терминал 3 — послать SIGINT во время звонка
kill -INT $(pgrep smiths-net)
```

Ожидаемо:

- `shutdown signal received; draining`
- Новые INVITE получают `503 Service Unavailable` + `Retry-After: 0`.
- Существующий звонок продолжается до hangup или таймаута drain.
- После drain: `graceful shutdown complete`, код `0`.

Автотест: `cargo test -p smiths-sip --test drain`.

---

## 27. Конференции

```bash
# Создать конференцию
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"create_conference","arguments":{"name":"room-1"}}}' \
  | jq '.result'

# Присоединить звонок
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"join_conference","arguments":{"conference_id":"<id>","call_id":"<call_id>"}}}' \
  | jq '.result'

# Отсоединить
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"leave_conference","arguments":{"conference_id":"<id>","call_id":"<call_id>"}}}' \
  | jq '.result'
```

---

## 28. DTMF

```bash
# Отправить DTMF в активный звонок
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"send_dtmf","arguments":{"call_id":"<id>","digits":"1234#"}}}' \
  | jq '.result'
```

С `media.inband_dtmf = true` — Goertzel-детектор на мосту
распознаёт входящие DTMF-тоны и публикует `SipEvent::Dtmf`.

---

## 29. HA-кластер

### 29.1 Primary / Secondary

```toml
# primary.toml
[cluster]
mode      = "primary"
peer_addr = "127.0.0.1:9001"

# secondary.toml
[cluster]
mode      = "secondary"
peer_addr = "127.0.0.1:9000"
```

```bash
# Терминал 1 — primary
./target/release/smiths-net --config primary.toml

# Терминал 2 — secondary
./target/release/smiths-net --config secondary.toml
```

Ожидаемо: при звонке через primary — secondary получает реплику
`DialogDelta::Upsert`. Ресурс `cluster://status` показывает роль
и peer connectivity.

### 29.2 Raft (multi-node)

```toml
[cluster]
mode          = "primary"
node_id       = 1
raft_addr     = "127.0.0.1:9100"
initial_peers = ["2@127.0.0.1:9101"]
```

Автотесты: `cargo test -p smiths-raft`.

---

## 30. Storage (CDR, recording, vector)

### 30.1 CDR

```toml
[storage]
backend = "sqlite"

[storage.sqlite]
path = "/tmp/smiths-storage.db"
```

После завершения звонка:

```bash
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_cdr","arguments":{}}}' \
  | jq '.result'
```

### 30.2 Recording

```toml
[storage.recording]
backend = "fs"

[storage.recording.fs]
root = "/tmp/smiths-recordings"
```

После звонка: `ls /tmp/smiths-recordings/` → `<call-id>.wav`.

### 30.3 Vector search

```toml
[storage.vector]
backend = "memory"
```

```bash
curl -s -X POST http://127.0.0.1:7880/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_calls_semantic","arguments":{"query":"hello","limit":5}}}' \
  | jq '.result'
```

---

## 31. Регресс-чек-лист перед merge в `main`

| # | Проверка | Ожидаемо |
|---|---|---|
| 1 | `cargo fmt --all -- --check` | без изменений |
| 2 | `cargo clippy --all-targets -- -D warnings` | `0 warnings` |
| 3 | `cargo test --workspace` | все зелёные |
| 4 | Запуск → `/health` 200 + deep JSON | ок |
| 5 | `/metrics` → OpenMetrics text | ок |
| 6 | `OPTIONS` → `200 OK` (п. 3.2) | ок |
| 7 | `MESSAGE` → `405` (п. 4) | ок |
| 8 | Дедуп ретрансмиссии (п. 5) | тот же `To;tag` |
| 9 | Мусорная датаграмма не роняет процесс (п. 6) | процесс жив |
| 10 | `SIGINT` → graceful drain + shutdown (п. 1.2 / 26) | код `0` |
| 11 | INVITE/ACK/BYE с pjsua (п. 10.2) | `CALL CONFIRMED` |
| 12 | 488 при несовместимом кодеке (п. 10.3) | `488` |
| 13 | `cargo test -p smiths-testkit --test audio_bridge` | WAV OK |
| 14 | TCP transport (п. 14) | pjsua TCP call OK |
| 15 | TLS transport: `cargo test -p smiths-sip --test tls` | зелёный |
| 16 | REGISTER + digest: `cargo test -p smiths-sip --test register` | зелёный |
| 17 | SRTP: `cargo test -p smiths-sip --test sdp_srtp` | зелёный |
| 18 | Rate limit: `cargo test -p smiths-sip --test rate_limit` | зелёный |
| 19 | WASM: `cargo test -p smiths-wasm` | зелёные |
| 20 | Sidecar: `cargo test -p smiths-plugin` | зелёные |
| 21 | `python3 examples/python-client/mcp_demo.py` (п. 12.2) | все вызовы OK |
| 22 | `python3 examples/python-client/a2a_demo.py` (п. 13.2) | agent card OK |
| 23 | MCP HTTP `tools/call health` (п. 12.7) | OK |
| 24 | MCP SSE events (п. 12.8) | `call/created` видно |
| 25 | MCP resources `config://current` (п. 12.9) | секреты redacted |
| 26 | MCP `put_config` log_level (п. 12.10) | filter reloaded |
| 27 | SIGHUP reload (п. 24) | canary window armed |
| 28 | `smiths-net validate` (п. 25.1) | код `0` |
| 29 | `smiths-net init --preset dev` (п. 25.3) | валидный TOML |
| 30 | UAC: `cargo test -p smiths-sip --test uac` | зелёный |
| 31 | WebRTC DTLS: `cargo test -p smiths-media --test dtls_handshake` | зелёный |
| 32 | ICE/TURN: `cargo test -p smiths-ice --test turn_allocation` | зелёный |
| 33 | Fuzz 1 min: `cargo +nightly fuzz run sip_parser -- -max_total_time=60` | 0 crashes |

---

## 32. Что ещё НЕ проверяется на этом этапе

Эти сценарии появятся по мере реализации:

- **SIP-over-QUIC** — config surface есть (`transports = ["quic"]`),
  runtime listener не подключён (scaffold).
- **WireGuard** — config surface есть (`sip.vpn.mode = "wireguard"`),
  runtime device не подключён (feature `wireguard`).
- **WebTransport** — config surface есть (`[webtransport]`),
  runtime scaffold.
- **MCP HTTP/3** — config surface есть (`[mcp.http3]`),
  runtime scaffold.
- **Полный re-INVITE / UPDATE / session-modification** — FSM готов,
  UAS handler не подключён.
- **Jitter-buffer** — учёт jitter есть в `StreamStats`,
  adaptive buffer не реализован.
- **Per-dialog 2xx INVITE retransmit loop** (RFC 3261 §13.3.1.4) —
  `invite_2xx_cache` обрабатывает простой retry.
- **Протобуф wire-формат для sidecar** — сейчас JSON-RPC,
  протобуф планируется для `on_rtp_frame`.
- **gRPC-over-UDS для sidecar** — feature `sidecar-grpc`.
- **Seccomp-BPF syscall filtering** — config surface есть
  (`plugins.sandbox.seccomp = "allowlist"`), runtime в процессе.
- **User-namespace / cgroups v2** для sandbox — в планах.
- **pcap tap** — feature `pcap`, config `observability.pcap_dir`.
- **Nightly 24h SIP fuzz** — запускается вручную.
- **`cargo bloat` бюджеты** — tracked.

Подробный план — в `docs/plans/00-roadmap.md` и `docs/plans/todo.md`.
