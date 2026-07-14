# Launch checklist

Everything to turn the repo into a landing page and run the launch. Work
top-to-bottom; the demo clip
([`demo-storyboard.md`](demo-storyboard.md)) gates everything else.

---

## 1. Repo settings (Settings → General, and the "About" gear on the repo home)

### About → Description (the one-liner under the repo name, ≤ 350 chars)

> AI-first SIP engine in Rust. A single static binary that speaks RFC 3261
> + RTP and lets an LLM answer, route, and bridge real phone calls through
> an embedded MCP server. Local-first AI (Whisper · llama.cpp · Silero) —
> answer a phone line fully offline.

### About → Topics (paste these)

```
sip  voip  telephony  rtp  webrtc  rust  llm  mcp
model-context-protocol  whisper  llama-cpp  voice-ai
speech-to-text  text-to-speech  self-hosted  offline  ai-agents
```

### About → Website

Point at the docs/marketing site if published (the repo ships a `site/`);
otherwise leave blank rather than linking a 404.

### Other toggles

- [ ] **Releases** enabled and shown in the sidebar (see §4).
- [ ] "Packages"/"Environments" hidden if empty (less clutter).
- [ ] Discussions on (optional) — a good home for "show me your dialplan"
      and support threads that would otherwise be noisy issues.

---

## 2. Social preview image (Settings → General → Social preview)

This is the card that renders when the repo link is pasted into
HN / Reddit / X / Slack. Without it you get a generic gray box.

- **Size:** 1280 × 640 px (2:1), PNG. Keep text in the centre safe area.
- **Content:** wordmark `smiths-net` + the one-liner
  *"An AI-first SIP engine in Rust — answer a phone line, fully offline."*
  Optional: the tiny ASCII pipeline (phone → engine → Whisper·llama.cpp·
  Silero) as a motif.
- **Style:** dark background, one accent colour, high contrast. A single
  frame grabbed from the hero demo also works well.
- Save the source under `docs/assets/social-preview.png` so it's
  versioned, then upload it in Settings.

---

## 3. README as a landing page

- [x] Hero slot reserved at the top (placeholder until the GIF lands).
- [x] One-line value prop + badges (CI, license, Rust, status).
- [x] "Why" differentiators, flagship-demo section, quickstart, plugin
      table, docs links.
- [ ] **Drop in `docs/assets/demo.gif`** and un-comment the `<img>` block.
- [ ] Sanity-check every quickstart command on a clean checkout.

---

## 4. Releases with prebuilt binaries

"Single static binary" only lands if it's one download, not a `cargo
build`. A release workflow already exists (`.github/workflows/release.yml`,
buildx multi-arch Docker).

- [ ] Cut a tagged release (e.g. `v0.73.0`) with attached binaries for
      linux-x86_64 / linux-arm64 / macos (and the Docker image).
- [ ] Add a one-line install to the README once the asset URLs exist
      (a `curl … | tar` one-liner, or `docker run`).
- [ ] Release notes = the CHANGELOG section for the tag.

---

## 5. Discoverability seeds (do before the big posts)

- [ ] Label 5–8 **`good first issue`**s (a plugin, a codec, a doc, a test)
      so drive-by stars can convert to contributors.
- [ ] Ensure `LICENSE`, `CONTRIBUTING.md`, `CHANGELOG.md` are present and
      linked (they are).
- [ ] Submit small PRs to **awesome lists**: `awesome-rust`,
      `awesome-selfhosted`, `awesome-mcp-servers`, and any `awesome-sip` /
      `awesome-voip`. These are slow-burn but compounding.

---

## 6. Launch sequence (space out over ~2–3 weeks — don't blast at once)

Ranked by fit. Each gets its own tailored title + a 3-line first comment
linking the **demo video** and a "why I built this." (Full post drafts are
a separate deliverable — ask for the launch-posts pack.)

| Order | Channel | Working title angle |
|------:|---------|---------------------|
| 1 | **r/LocalLLaMA** | "I built a phone line answered by a fully-local AI (Whisper + llama.cpp + Silero) — Rust SIP engine, MCP-driven" |
| 2 | **Show HN** | "Show HN: smiths-net — an AI-first SIP engine in Rust that answers calls offline" |
| 3 | **r/selfhosted** / r/homelab | "Self-host a private phone AI — no cloud, runs on one GPU" |
| 4 | **This Week in Rust** (submit) + **lobste.rs** | single-binary Rust engine, two-tier plugin ABI, `unsafe`-free |
| 5 | **r/VOIP** / r/asterisk / Kamailio · FreeSWITCH forums | "An AI-native alternative to Asterisk/FreeSWITCH (early, MIT-spirit Apache-2.0)" |
| 6 | **MCP ecosystem** | first telephony MCP server → awesome-mcp-servers, MCP directories, Anthropic Discord |

First-comment skeleton (adapt per channel):

```
Hi — author here. smiths-net is a small Rust SIP engine; the twist is an
embedded MCP server, so an LLM agent drives real calls, and ASR/LLM/TTS
are plugins you can run fully locally. Demo (30s): <link>. It's pre-1.0 —
feedback, especially from telephony folks, very welcome. Happy to answer
anything.
```

### Posting hygiene

- [ ] Lead with the **video**, not the repo link (repos convert poorly cold).
- [ ] Post mid-week, morning US-Eastern, for HN/Reddit reach.
- [ ] Reply to every comment in the first 2 hours — engagement drives rank.
- [ ] Be upfront about **pre-1.0** status; don't oversell to the VoIP crowd
      (they test rigorously — underpromise, let the demo overdeliver).
- [ ] Never argue with skeptics; thank, note, file an issue.

---

## 7. Compounding (after the spike)

- [ ] A build-log post — *"I taught a phone line to run a local LLM"* — on
      a personal blog + dev.to, cross-linked from the README. Durable, and
      it feeds future threads.
- [ ] Cut the hero clip into a 15 s vertical for X / Bluesky / YouTube
      Shorts.
- [ ] Pin the demo + roadmap in Discussions; convert recurring questions
      into README FAQ entries.
