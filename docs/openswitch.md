Ниже представлен минимальный тезисный набор требований к ядру SIP-клиента/движка на Rust, ориентированного на запуск как MCP-сервер с поддержкой динамических WASM-плагинов. Набор отсекает PBX-избыточность FreeSWITCH, оставляя только фундамент для сигнализации, медиа, расширяемости и AI-интеграции.

### 1. SIP-сигнализация (RFC 3261 + базовые расширения)
- **Транспорты:** UDP, TCP, TLS 1.2+ (настройка на уровне endpoint)
- **Методы:** `INVITE`, `ACK`, `BYE`, `CANCEL`, `OPTIONS`, `REGISTER`, `PRACK` (опц.), `UPDATE` (опц.)
- **Коды ответов:** `100`, `180`, `183`, `200`, `401`, `407`, `486`, `487`, `500`+ (базовая классификация)
- **Заголовки:** обязательная парсинг/генерация `Via`, `Call-ID`, `CSeq`, `From`, `To`, `Contact`, `Max-Forwards`, `Route`, `Record-Route`, `Content-Type`, `Content-Length`
- **Транзакции и диалоги:** конечный автомат (Idle → Early → Confirmed → Terminated), ретрансмиссии (RFC 3261 timers A/B/C/D/E/F/G/H/I), сопоставление транзакций по Via/Call-ID/CSeq
- **SDP обмен:** парсер/генератор базовых полей (`v=`, `o=`, `s=`, `c=`, `t=`, `m=audio`, `a=rtpmap`, `a=sendrecv/recvonly/inactive`), оффер/ансвер логика без сложных атрибутов
- **Аутентификация:** Digest (MD5/SHA-256), обработка `401/407`, кеш nonce, поддержка `qop=auth`

### 2. Медиа-тракт (RTP/RTCP)
- **RTP:** отправка/приём UDP-пакетов, корректные sequence numbers, timestamp, SSRC/CSRC, payload type mapping
- **RTCP:** минимальные `RR`/`SR` для статистики (packet loss, jitter, RTT), периодическая отправка
- **Кодеки:** PCMU (0), PCMA (8), Opus (111) – перекодировка не обязательна в минимуме, только passthrough/negotiation
- **Jitter buffer:** простой фиксированный или одноуровневый адаптивный (до 120 мс), без FEC/PLC
- **Маршрутизация медиа:** проброс между локальным socket и плагином/внешним peer без микширования или конференций

### 3. Архитектура ядра
- **Async runtime:** `tokio` (multi-thread), неблокирующие IO, таймеры, каналы событий
- **Event bus:** единая шина с типизированными событиями `SipEvent`, `MediaEvent`, `ControlEvent`, `PluginEvent`
- **State machine вызовов:** явный FSM с переходами, хранением метаданных (peer, codecs, local/remote SDP, RTP ports, state)
- **Конфигурация:** TOML/YAML, валидация схемой, hot-reload базовых параметров (bind, codecs, TLS paths, plugin dir)
- **Логирование:** `tracing` с уровнями, структурированный вывод, корреляция по `call_id`/`transaction_id`
- **Graceful shutdown:** корректное завершение активных вызовов, сохранение метрик, закрытие socket/WASM context

### 4. WASM-плагины
- **Исполнитель:** `wasmtime` (изоляция, sandbox, контроль памяти/CPU)
- **ABI плагинов:** чёткие хуки:
  - `on_init(config) → Result`
  - `on_sip_request(method, headers, body) → (action, modified_msg)`
  - `on_sdp_offer/answer(sdp) → Result`
  - `on_rtp_frame(ssrc, pt, data) → (action, modified_frame?)`
  - `on_call_state_change(old, new, meta) → ()`
  - `on_timer(id) → ()`
- **Host functions (syscall):** `log(level, msg)`, `send_sip(msg)`, `send_rtp(data)`, `set_timer(ms)`, `get_call_meta(key)`, `store_plugin_state(key, value)`
- **Жизненный цикл:** динамическая загрузка `.wasm` без рестарта, валидация манифеста (имя, версия, требуемые хуки, permissions), выгрузка с завершением активных контекстов
- **Ограничения:** лимиты памяти (до 64 МБ), лимит CPU per call, запрет на прямой network I/O (только через host functions)

### 5. MCP-интеграция (Model Context Protocol)
- **Транспорт:** `stdio` (по умолчанию для AI-агентов) или `HTTP/SSE`
- **Инструменты (Tools):**
  - `make_call(destination, from, codecs?) → call_id`
  - `end_call(call_id, reason?) → status`
  - `get_call_status(call_id) → state, duration, media_stats`
  - `list_plugins() → [name, version, status]`
  - `load_plugin(path, config?) → plugin_id`
  - `unload_plugin(plugin_id) → status`
- **Ресурсы (Resources):**
  - `sip://calls/{id}` (JSON с текущим состоянием и SDP)
  - `plugin://manifests/{name}` (описание хуков и конфигурации)
  - `config://current` (активная конфигурация ядра)
- **Промпт-интеграция:** системное описание доступных инструментов, ограничений и ожидаемых форматов ответов для LLM
- **Безопасность MCP:** валидация входных параметров, rate-limit на вызовы инструментов, аутентификация (опц.), audit-log действий

### 6. Безопасность и эксплуатация
- **TLS/DTLS:** поддержка TLS для SIP, опционально SRTP (minimally: passthrough encrypted RTP without key negotiation)
- **Валидация входящих:** проверка длин полей, санитайзинг SDP, защита от buffer/integer overflow, reject malformed SIP
- **Метрики:** Prometheus-совместимые counters/gauges (`active_calls`, `sip_transactions`, `plugin_errors`, `rtp_packets_in/out`)
- **Health-check:** `GET /health` или MCP-ресурс, проверка livelock/deadlock, состояние WASM runtime
- **Отладка:** pcap-логирование (опц.), дампы SDP/SIP, verbose tracing для плагинов

### 7. Явно НЕ входит в минимальную версию
- Видеозвонки, конференции, микширование, транскодинг в реальном времени
- Сложный диалплан/IVR, базы данных абонентов, HA/кластеризация
- ICE/STUN/TURN, полная NAT traversal логика
- FAX (T.38), DTMF relay (RFC 2833/INBAND можно добавить позже)
- Распределённые сессии, replication, persistent storage состояния

### Рекомендуемый стек (Rust)
- `tokio`, `tokio-util`, `bytes`
- `sdp-rs` (кастомный парсер или `webrtc-sdp`)
- `rtp`, `rtcp` (crate `webrtc-rs` или `rtp-rs`/`rtcp-rs`)
- `wasmtime` + `wasmtime-cranelift`
- `mcp-sdk` (официальный Rust SDK или ручная реализация по спецификации)
- `tracing`, `tracing-subscriber`, `serde`, `toml`/`yaml`
- `rustls`/`tokio-rustls` для TLS

Этот набор обеспечивает работающий SIP-ядро с базовым сигнальным и медиа-трактом, изолированной расширяемостью через WASM и готовым интерфейсом для AI-агентов через MCP. Все компоненты спроектированы для горизонтального масштабирования: сначала добавляется ядро и 1–2 плагина, затем расширяются кодеки, медиалоги, сложные хуки и MCP-инструменты.