# Open-Source Strategy for smiths-net

Notes on how to take **smiths-net** from a private repo (already
Apache-2.0 licensed) to a healthy open-source project — what to do,
what to avoid, what to decide first. Bilingual: English first, then
Russian (same structure, not a machine translation).

---

# English

## 0. Starting position

- Project is **already Apache-2.0**: permissive, includes a patent grant,
  compatible with nearly every commercial use. "Open-sourcing" is
  therefore not about changing the license — it's about **launch
  strategy, governance, and revenue model**.
- Small team (Mindhalla). Need a strategy that:
  - Does not require heavy legal overhead (no CLA treadmill on day one).
  - Attracts early contributions quickly (plugin authors especially).
  - Keeps a clear path to commercial revenue later.
  - Does not alienate carriers / telcos — they typically refuse
    copyleft and need support SLAs.

## 1. Strategic options — landscape

Five realistic license/strategy families. Only one becomes the
**primary** strategy; others may appear as components (e.g. a
commercial enterprise plugin on top of a permissive core).

### A. Pure permissive (Apache-2.0 — current)

- **Who**: MIT, Apache 2.0, BSD-3.
- **Pros**: maximum adoption; no friction for embedders, telcos, or
  cloud vendors; well-understood legal terms; Apache-2.0 includes an
  explicit patent grant (important in telephony).
- **Cons**: cannot capture value via the license itself. Anyone —
  including hyperscalers — can repackage and sell it. Monetization
  must come from elsewhere (managed service, support, commercial
  plugins, consulting).
- **Who lives here successfully**: Rust itself, Kubernetes (+ CNCF),
  drachtio-server, baresip, PJSIP. Jambonz (MIT) is the closest AI-
  first SIP analog.

### B. Weak copyleft (MPL-2.0 / LGPL-3.0)

- **Pros**: modifications to the core must be shared back, but
  downstream can still link proprietary code against it.
- **Cons**: patch-level copyleft boundary is confusing for operators;
  adoption is smaller than permissive; legal teams at telcos push back.
- **Precedent**: FreeSWITCH (MPL-1.1 historically).

### C. Strong copyleft (GPL-3.0 / AGPL-3.0)

- **Pros**: all derivatives — including SaaS under AGPL — must release
  their changes. Protects against "hyperscaler takes our code and
  ships it as a closed managed service."
- **Cons**: **poison for telecom adoption.** Carriers, enterprise
  PBX vendors, and most SIP equipment vendors will refuse on
  principle. Plugin authors also hesitate (AGPL propagates).
- **Precedent**: Asterisk (GPLv2, with Sangoma selling a commercial
  exception), Kamailio (GPLv2).

### D. Open core (permissive core + commercial add-ons)

- **Pros**: common, understood, lets a small team fund itself via
  enterprise features without harming the core community.
- **Cons**: requires discipline about what stays open vs closed; if
  the "open" core is too thin, community trust erodes (see
  long-running Elastic / Redis debates).
- **Precedent**: GitLab (MIT core + EE), HashiCorp pre-BSL (MPL + EE),
  Grafana, MinIO.

### E. Source-available (BSL 1.1 / SSPL / Elastic License 2.0)

- **Pros**: legally prevents hyperscaler "strip-mine and sell"
  scenarios; under BSL the code auto-converts to OSI-open after a
  fixed horizon (commonly 3–4 years).
- **Cons**: **not OSI-approved** — the community will correctly call
  it source-available, not open source; many distros, Fedora/Debian,
  won't ship it; it signals distrust of the ecosystem.
- **Precedent**: MariaDB (BSL → GPL), Redpanda (BSL), HashiCorp
  (late-stage BSL), Elastic (SSPL/ELv2), MongoDB (SSPL).

## 2. Telecom / SIP ecosystem — precedent

| Project        | License         | Revenue model                              |
|----------------|-----------------|--------------------------------------------|
| Asterisk       | GPLv2 + dual    | Sangoma hardware + commercial license sales|
| FreeSWITCH     | MPL-1.1         | SignalWire cloud, support, enterprise deals|
| Kamailio       | GPLv2           | Consultancy, integrators, trainings        |
| drachtio       | MIT             | Jambonz cloud + enterprise support         |
| Jambonz        | MIT             | `jambonz.cloud` managed service            |
| PJSIP          | GPLv2 / commercial | Commercial dual license, consulting     |
| baresip        | BSD-3           | Consulting, niche embedders                |
| Twilio Voice   | closed          | Pure SaaS                                  |

**Observation**: the projects that **grew** in the last five years are
either **permissive + cloud** (Jambonz, drachtio) or **proprietary
cloud** (Twilio). The copyleft veterans (Asterisk, FreeSWITCH,
Kamailio) plateaued in new-project adoption — they are kept alive by
their installed base and a small number of commercial sustainers. For
an **AI-first** engine in 2026, permissive + cloud is the trajectory
with live momentum.

## 3. Recommended strategy for smiths-net

### Phase A — pre-v1.0 (now → community seed)

Keep the current posture:

- **License**: Apache-2.0 **unchanged**. Do not switch to AGPL / BSL /
  SSPL before product-market fit; that decision can always be made
  later, never un-made.
- **Contributor Agreement**: **DCO sign-off only** (`Signed-off-by:`
  in commits). No CLA. A CLA is heavyweight and slows first
  contributions; DCO is industry-standard and sufficient unless you
  later need to relicense.
- **Trademark**: register "smiths-net" (and any chosen name) early.
  Trademarks are how permissive projects retain identity — anyone
  can fork the code, only you can ship "the official smiths-net."
  Budget: a few hundred USD per jurisdiction; file at least in your
  home jurisdiction and in the US (for CNCF-adjacent visibility).
- **Governance**: benevolent-maintainer model. One `CODEOWNERS`
  file, two named maintainers with merge rights. Document in
  `GOVERNANCE.md` that this is the starting model and will evolve.
- **Repo hygiene for launch**:
  - `LICENSE` (Apache-2.0 — present)
  - `NOTICE` file listing third-party attributions (generate with
    `cargo-about` or `cargo-deny`)
  - `SECURITY.md` with disclosure process + PGP / Signal contact
  - `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1)
  - `CONTRIBUTING.md` (present — refine with DCO instructions)
  - Issue + PR templates
  - `SUPPORT.md` — where to ask questions (GitHub Discussions)

### Phase B — around v1.0 (if traction appears)

Once real users show up (1k+ GitHub stars, a handful of production
deployments, a non-trivial plugin ecosystem):

- **Keep core Apache-2.0.** Explicitly commit to this in `GOVERNANCE.md`
  to build trust (see Elastic / HashiCorp relicense backlash for why).
- **Offer a commercial plugin bundle** under a separate license:
  - Clustering / HA (post-MVP P15)
  - Advanced observability (Datadog, Dynatrace, Splunk connectors)
  - Compliance-grade audit logging
  - SLA-backed AI provider connectors (OpenAI, Anthropic, Bedrock)
  - These live in a **separate repository** so the main repo stays
    100 % Apache. Plugins communicate only via the public WASM /
    sidecar ABI.
- **Launch a managed cloud** (`smiths.cloud` or similar). Zero-ops
  deploys of the exact same binary; monthly subscription for
  multi-tenant. This is the highest-margin revenue channel — Jambonz,
  Twilio, Supabase, ClickHouse all validate it.

### Phase C — if hyperscaler pressure materializes

Only relevant if AWS / Google / Azure start shipping smiths-net as a
first-party managed service and capturing the revenue. Then:

- **Do not jump to AGPL or SSPL.** Both damage the ecosystem. Instead:
  - Double down on trademark enforcement (hyperscaler can't call
    theirs "smiths-net").
  - Double down on managed-cloud velocity — hyperscalers are
    usually 1–2 years behind on feature parity with the original.
  - Invest in a **certification / "official plugin" mark** — carriers
    buy certified integrations; hyperscaler derivatives are not.
- If eventually necessary, **BSL with a 3-year conversion clock** is
  the least harmful source-available move — MariaDB's model is the
  most community-legitimate of this family. But defer this decision
  until real evidence of extraction exists; many projects fear it
  and never actually see it.

## 4. Things to get right at launch

### Legal + IP

- **Copyright**: each file header stays `Copyright YYYY Mindhalla
  (or contributors)` under Apache-2.0. No "assign to us" clause.
- **Patent grant**: inherent in Apache-2.0. Good. Do **not** dual-
  license with MIT-only — MIT lacks an explicit patent grant, which
  matters in telephony where patent pools (G.729, H.264, Opus) exist.
- **Export control**: SIP engines touch crypto (TLS, SRTP). Under US
  EAR, open-source crypto published to a public repo is exempt from
  most controls once notification is sent (5D002 + Note 3). File a
  one-time notice with BIS when publishing. Non-US maintainers
  should check their local equivalent (Wassenaar).
- **CLA decision**: DCO is sufficient. Revisit only if you need to
  relicense the whole codebase later.

### Community + adoption

- **Ship a `docker run` quickstart** so people can try in 60 seconds.
- **Benchmark publicly** against Kamailio / FreeSWITCH on OPTIONS /
  REGISTER / INVITE throughput. Numbers persuade telecom engineers.
- **Write three high-signal blog posts before launch**:
  1. "Why we built a new Rust SIP engine in 2026" (positioning).
  2. "Plugins in WASM, Lua, and sidecar — one ABI, three languages"
     (the differentiator).
  3. "Making an LLM answer the phone in 40 lines of Python" (viral
     demo — maps to the existing `examples/python-client`).
- **Show up where SIP engineers are**: ClueCon, Kamailio World,
  Astricon, IETF SIPCORE mailing list, r/VOIP, Hacker News.
- **Reference integrations**: a "known-good" recipe with Kamailio
  (front) and Asterisk / FreeSWITCH (bridged) as a B2BUA removes the
  biggest adoption objection — "we already have X."

### Operational

- **Sign releases** with sigstore / cosign. Telcos increasingly
  demand signed artifacts.
- **SBOM** on every release (`cargo-sbom` / `cyclonedx-bom`).
- **Security disclosures** via `security@` mail alias; publish a PGP
  key; commit to a 90-day embargo default.
- **Deprecation policy**: one minor release of warning before any
  breaking change, documented in `CHANGELOG.md`.
- **Versioning**: SemVer. Below 1.0 you have latitude — use it; do
  not 1.0 too early.

## 5. Risks and mitigations

| Risk                                                   | Mitigation                                              |
|--------------------------------------------------------|---------------------------------------------------------|
| Hyperscaler repackages & captures revenue              | Trademark; managed-cloud velocity; certification mark   |
| Fragmentation / unfriendly forks                       | Clear governance; responsive maintainers; trademark     |
| Contributor burnout                                    | Paced release cadence; "good first issue" pipeline; docs|
| Licensing confusion (Apache vs plugins' licenses)      | Per-plugin `LICENSE` files; top-level `LICENSES/` index |
| Patent litigation                                      | Apache-2.0 patent grant + termination clause; DCO       |
| Carrier rejection of anything copyleft-adjacent        | Stay permissive; never let AGPL code into the main tree |
| Export-control surprise                                | File BIS notice on first crypto-containing release      |
| Relicense backlash (Elastic / HashiCorp scenario)      | Commit in `GOVERNANCE.md` to keep core Apache           |

## 6. Concrete launch checklist

Before flipping the repo public / announcing:

- [ ] `LICENSE` — Apache-2.0 present and unmodified
- [ ] `NOTICE` — generated + committed
- [ ] `SECURITY.md` — contact, PGP, embargo policy
- [ ] `CODE_OF_CONDUCT.md` — Contributor Covenant 2.1
- [ ] `CONTRIBUTING.md` — DCO instructions, dev loop, style
- [ ] `GOVERNANCE.md` — maintainer list, decision process, license
      commitment
- [ ] `SUPPORT.md` — where to get help
- [ ] Issue & PR templates
- [ ] Trademark filed (or docketed) in home jurisdiction
- [ ] `docker run`-ready image on a public registry
- [ ] Benchmark repo or document vs Kamailio / FreeSWITCH
- [ ] Three launch blog posts drafted
- [ ] BIS crypto notification filed (if US)
- [ ] Signed releases configured (sigstore / cosign in CI)
- [ ] SBOM generation in release pipeline
- [ ] GitHub Discussions enabled
- [ ] Domain registered, basic landing page live
- [ ] Plan for first 90 days of community responses (who answers
      what, cadence)

## 7. Decision tree — one page

```
Goal = maximum adoption, plugin ecosystem growth
 └─► Apache-2.0 core + commercial add-on plugins + managed cloud
    (RECOMMENDED for smiths-net)

Goal = prevent hyperscaler strip-mining above all else
 └─► Start Apache-2.0; if/when extraction actually happens,
    BSL the *new* major version with a 3-year conversion clock.
    Older versions stay Apache forever.

Goal = force all derivatives to contribute back
 └─► AGPL-3.0  (NOT recommended — kills telco adoption)

Goal = sell commercial licenses as primary revenue
 └─► GPLv2 + commercial exception, Asterisk-style
    (NOT recommended — small team, 2026 market prefers permissive)
```

---

# Русский

## 0. Исходная точка

- Проект **уже под Apache-2.0**: permissive-лицензия с явным патентным
  грантом, совместима почти с любым коммерческим применением.
  «Открыть исходники» — это не про смену лицензии, а про **стратегию
  запуска, governance и модель монетизации**.
- Команда маленькая (Mindhalla). Стратегия должна:
  - Не требовать большой юридической нагрузки (никаких CLA в первый
    день).
  - Быстро привлекать внешние contributions (особенно авторов
    плагинов).
  - Оставить ясный путь к коммерческой выручке позже.
  - Не отпугнуть операторов связи / телеком-компании — они почти
    всегда отказываются от copyleft и требуют SLA.

## 1. Варианты стратегии

Пять реалистичных семейств лицензий / стратегий. Одно станет
**основным**; остальные могут появляться как компоненты (например,
коммерческий enterprise-плагин поверх permissive-ядра).

### A. Чистый permissive (Apache-2.0 — текущий)

- **Кто**: MIT, Apache 2.0, BSD-3.
- **Плюсы**: максимум принятия; никакого трения для вендоров,
  операторов или облаков; понятные юридические условия; Apache-2.0
  содержит явный патентный грант (важно в телефонии).
- **Минусы**: через саму лицензию деньги не извлечёшь. Кто угодно,
  включая гиперскейлеров, может переупаковать и продавать.
  Монетизация приходит откуда-то ещё — managed service, поддержка,
  коммерческие плагины, консалтинг.
- **Успешные примеры**: сам Rust, Kubernetes (+ CNCF),
  drachtio-server, baresip, PJSIP. Jambonz (MIT) — ближайший аналог в
  AI-first SIP.

### B. Слабый copyleft (MPL-2.0 / LGPL-3.0)

- **Плюсы**: модификации ядра надо возвращать в upstream, но
  downstream может линковать проприетарный код.
- **Минусы**: граница «по файлам» запутанная для операторов;
  распространение меньше, чем у permissive; юристы телекома сопротивляются.
- **Прецедент**: FreeSWITCH (исторически MPL-1.1).

### C. Сильный copyleft (GPL-3.0 / AGPL-3.0)

- **Плюсы**: все производные — включая SaaS под AGPL — обязаны
  отдавать изменения. Защищает от «гиперскейлер забрал нас и продаёт
  как закрытый managed service».
- **Минусы**: **яд для телеком-принятия.** Операторы, PBX-вендоры и
  большинство производителей SIP-оборудования отказывают принципиально.
  Авторы плагинов тоже колеблются (AGPL «заражает» вниз).
- **Прецедент**: Asterisk (GPLv2 + Sangoma продаёт коммерческое
  исключение), Kamailio (GPLv2).

### D. Open core (permissive-ядро + коммерческие надстройки)

- **Плюсы**: распространено, понятно, позволяет маленькой команде
  финансироваться за счёт enterprise-фич без ущерба для open-сообщества.
- **Минусы**: требует дисциплины, что остаётся открытым, а что — нет;
  если «открытое» ядро слишком тощее, доверие сообщества рушится
  (см. многолетние дискуссии вокруг Elastic / Redis).
- **Прецедент**: GitLab (MIT core + EE), HashiCorp до BSL (MPL + EE),
  Grafana, MinIO.

### E. Source-available (BSL 1.1 / SSPL / Elastic License 2.0)

- **Плюсы**: юридически предотвращает сценарий «гиперскейлер-отжим»;
  BSL автоматически конвертируется в OSI-open через фиксированный
  срок (обычно 3–4 года).
- **Минусы**: **не OSI-approved** — сообщество справедливо назовёт
  это source-available, не open source; Fedora/Debian и многие
  дистрибутивы не примут; сигнализирует недоверие к экосистеме.
- **Прецедент**: MariaDB (BSL → GPL), Redpanda (BSL), HashiCorp
  (позднее BSL), Elastic (SSPL/ELv2), MongoDB (SSPL).

## 2. Экосистема телекома / SIP — прецеденты

| Проект          | Лицензия            | Модель выручки                              |
|-----------------|---------------------|---------------------------------------------|
| Asterisk        | GPLv2 + dual        | Sangoma hardware + продажа коммерч. лицензий|
| FreeSWITCH      | MPL-1.1             | SignalWire cloud, поддержка, enterprise     |
| Kamailio        | GPLv2               | Консалтинг, интеграторы, тренинги           |
| drachtio        | MIT                 | Jambonz cloud + enterprise support          |
| Jambonz         | MIT                 | `jambonz.cloud` managed service             |
| PJSIP           | GPLv2 / commercial  | Коммерческая dual license, консалтинг       |
| baresip         | BSD-3               | Консалтинг, нишевые встройки                |
| Twilio Voice    | closed              | Чистый SaaS                                 |

**Наблюдение**: проекты, которые **росли** последние пять лет, —
это либо **permissive + облако** (Jambonz, drachtio), либо
**проприетарное облако** (Twilio). Copyleft-ветераны (Asterisk,
FreeSWITCH, Kamailio) вышли на плато в новых внедрениях — их держит
установленная база и несколько коммерческих спонсоров. Для
**AI-first** движка в 2026 году траектория с живым импульсом —
permissive + облако.

## 3. Рекомендованная стратегия для smiths-net

### Фаза A — до v1.0 (сейчас → посев сообщества)

Сохранить текущую позицию:

- **Лицензия**: Apache-2.0 **без изменений**. Не переключаться на
  AGPL / BSL / SSPL до подтверждения product-market fit; это решение
  можно принять позже, но нельзя отменить.
- **Contributor Agreement**: **только DCO sign-off** (`Signed-off-by:`
  в коммитах). Никакого CLA. CLA — тяжёлый механизм, тормозит первые
  contributions; DCO — индустриальный стандарт, и его достаточно,
  пока не нужна будущая массовая relicense.
- **Товарный знак**: зарегистрировать «smiths-net» (и любое выбранное
  имя) как можно раньше. Товарный знак — то, как permissive-проекты
  удерживают идентичность: форкать код может кто угодно, но
  поставлять «официальный smiths-net» — только вы. Бюджет — несколько
  сотен USD на юрисдикцию; подать хотя бы в своей стране и в США
  (для видимости рядом с CNCF).
- **Governance**: модель «доброжелательный мейнтейнер». Один файл
  `CODEOWNERS`, два именованных мейнтейнера с правом merge.
  Зафиксировать в `GOVERNANCE.md`, что это стартовая модель и она
  будет эволюционировать.
- **Гигиена репозитория к запуску**:
  - `LICENSE` (Apache-2.0 — уже есть)
  - `NOTICE` со списком атрибуций third-party (генерировать через
    `cargo-about` или `cargo-deny`)
  - `SECURITY.md` с процедурой disclosure + PGP / Signal контакт
  - `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1)
  - `CONTRIBUTING.md` (уже есть — дополнить инструкциями по DCO)
  - Шаблоны issues + PR
  - `SUPPORT.md` — где задавать вопросы (GitHub Discussions)

### Фаза B — около v1.0 (если появляется traction)

Когда появятся реальные пользователи (1 тыс.+ GitHub stars, несколько
production-внедрений, нетривиальная экосистема плагинов):

- **Ядро оставить под Apache-2.0.** Явно зафиксировать это в
  `GOVERNANCE.md` для доверия (см. историю ребренда Elastic /
  HashiCorp как пример того, почему это важно).
- **Выпустить коммерческий набор плагинов** под отдельной лицензией:
  - Кластеризация / HA (post-MVP P15)
  - Продвинутая observability (Datadog, Dynatrace, Splunk)
  - Аудит уровня compliance
  - AI-провайдеры с SLA (OpenAI, Anthropic, Bedrock)
  - Эти плагины живут в **отдельном репозитории**, основной остаётся
    100 % Apache. Общение только через публичный WASM / sidecar ABI.
- **Запустить managed cloud** (`smiths.cloud` или подобное).
  Zero-ops-развёртывание того же бинарника; месячная подписка на
  multi-tenant. Это самый маржинальный канал — Jambonz, Twilio,
  Supabase, ClickHouse все подтверждают это.

### Фаза C — если появится давление от гиперскейлеров

Актуально только если AWS / Google / Azure начнут поставлять
smiths-net как first-party managed service, забирая выручку. Тогда:

- **Не прыгать на AGPL или SSPL.** Обе навредят экосистеме. Вместо
  этого:
  - Усилить защиту товарного знака (гиперскейлер не сможет назвать
    свой продукт «smiths-net»).
  - Усилить скорость managed cloud — гиперскейлеры обычно отстают на
    1–2 года по фиче-паритету с оригиналом.
  - Ввести «сертификацию / знак официального плагина» — операторы
    покупают сертифицированные интеграции; у гиперскейлера таких нет.
- Если всё же понадобится — **BSL с 3-летним таймером конверсии** —
  наименее вредоносный source-available-шаг. Модель MariaDB — самая
  community-легитимная в этом семействе. Но отложить решение до
  реальных доказательств extraction; многие проекты боятся этого
  сценария и никогда с ним не сталкиваются.

## 4. Что важно сделать правильно при запуске

### Юридическое + IP

- **Copyright**: заголовок каждого файла — `Copyright YYYY Mindhalla
  (or contributors)` под Apache-2.0. Никаких «переуступить нам».
- **Патентный грант**: уже встроен в Apache-2.0. Хорошо. **Не** делать
  dual-license с MIT — MIT не содержит явного патентного гранта,
  а в телефонии это важно (патентные пулы на G.729, H.264, Opus).
- **Экспортный контроль**: SIP-движки касаются крипто (TLS, SRTP). По
  американскому EAR open-source крипто, опубликованное в публичном
  репозитории, освобождается от большинства контролей после
  уведомления (5D002 + Note 3). Подать одноразовое уведомление в BIS
  при публикации. Не-американским мейнтейнерам — проверить локальный
  аналог (Wassenaar).
- **Решение по CLA**: DCO достаточно. Пересмотреть, только если
  потом понадобится массовая relicense всей кодовой базы.

### Сообщество + распространение

- **Quickstart через `docker run`** — чтобы попробовать за 60 секунд.
- **Публичные бенчмарки** против Kamailio / FreeSWITCH на
  OPTIONS / REGISTER / INVITE throughput. Цифры убеждают
  телеком-инженеров.
- **Три сильных поста в блоге перед запуском**:
  1. «Почему мы сделали новый Rust SIP-движок в 2026 году»
     (позиционирование).
  2. «Плагины в WASM, Lua и sidecar — один ABI, три языка»
     (дифференциатор).
  3. «Заставляем LLM отвечать на телефон в 40 строках Python»
     (вирусная демка — ложится на существующий
     `examples/python-client`).
- **Быть там, где SIP-инженеры**: ClueCon, Kamailio World, Astricon,
  рассылка IETF SIPCORE, r/VOIP, Hacker News.
- **Референс-интеграции**: «известно-работающий» рецепт с Kamailio
  (фронтом) и Asterisk / FreeSWITCH (как B2BUA) снимает главный
  аргумент против принятия — «у нас уже есть X».

### Операционное

- **Подписывать релизы** через sigstore / cosign. Операторы всё
  чаще требуют подписанных артефактов.
- **SBOM** на каждый релиз (`cargo-sbom` / `cyclonedx-bom`).
- **Security disclosures** через email-алиас `security@`; публиковать
  PGP-ключ; по умолчанию 90-дневное эмбарго.
- **Политика deprecation**: минимум один minor-релиз предупреждения
  перед любым breaking change, задокументировано в `CHANGELOG.md`.
- **Версионирование**: SemVer. До 1.0 есть свобода — пользоваться ею;
  не выпускать 1.0 слишком рано.

## 5. Риски и митигации

| Риск                                                   | Митигация                                              |
|--------------------------------------------------------|--------------------------------------------------------|
| Гиперскейлер переупаковал и забрал выручку             | Товарный знак; скорость managed cloud; сертификация    |
| Фрагментация / недружественные форки                   | Ясное governance; отзывчивые мейнтейнеры; товарный знак|
| Выгорание contributors                                 | Разумный темп релизов; pipeline «good first issue»; доки|
| Путаница с лицензиями (Apache ядро vs плагины)         | Per-plugin `LICENSE` файлы; каталог `LICENSES/` сверху |
| Патентные иски                                         | Apache-2.0 patent grant + termination clause; DCO      |
| Отказ операторов от любого copyleft-намёка             | Оставаться permissive; не впускать AGPL в main-дерево  |
| Сюрприз с экспортным контролем                         | Подать уведомление BIS при первом релизе с крипто      |
| Бэклеш от relicense (сценарий Elastic / HashiCorp)     | Зафиксировать в `GOVERNANCE.md`, что ядро остаётся Apache |

## 6. Конкретный чек-лист запуска

Перед тем как делать репозиторий публичным / анонсировать:

- [ ] `LICENSE` — Apache-2.0, неизменённый
- [ ] `NOTICE` — сгенерирован и закоммичен
- [ ] `SECURITY.md` — контакт, PGP, политика эмбарго
- [ ] `CODE_OF_CONDUCT.md` — Contributor Covenant 2.1
- [ ] `CONTRIBUTING.md` — инструкции по DCO, dev loop, стиль
- [ ] `GOVERNANCE.md` — список мейнтейнеров, процесс решений,
      обязательство по лицензии
- [ ] `SUPPORT.md` — где получить помощь
- [ ] Шаблоны issues и PR
- [ ] Товарный знак подан (или в очереди) в домашней юрисдикции
- [ ] Готовый `docker run`-образ в публичном реестре
- [ ] Репозиторий или документ с бенчмарками vs Kamailio / FreeSWITCH
- [ ] Три launch-поста в черновике
- [ ] Уведомление в BIS по крипто подано (если США)
- [ ] Подпись релизов настроена (sigstore / cosign в CI)
- [ ] Генерация SBOM в pipeline релиза
- [ ] GitHub Discussions включены
- [ ] Домен зарегистрирован, минимальная landing page живая
- [ ] План первых 90 дней ответов community (кто и на что отвечает,
      с какой каденцией)

## 7. Дерево решения — на одну страницу

```
Цель = максимум принятия, рост экосистемы плагинов
 └─► Apache-2.0 ядро + коммерческие add-on плагины + managed cloud
    (РЕКОМЕНДУЕТСЯ для smiths-net)

Цель = любой ценой предотвратить отжим гиперскейлерами
 └─► Старт с Apache-2.0; если/когда extraction реально случится,
    перевести *новую* major-версию под BSL с 3-летним таймером.
    Старые версии остаются Apache навсегда.

Цель = заставить все производные возвращать изменения
 └─► AGPL-3.0  (НЕ рекомендуется — убивает телеком-принятие)

Цель = продажа коммерческих лицензий как основной источник выручки
 └─► GPLv2 + коммерческое исключение в стиле Asterisk
    (НЕ рекомендуется — маленькая команда, рынок 2026 предпочитает permissive)
```
