# Hero demo — storyboard & capture guide

The single most important launch asset: a short clip proving
**"a fully local AI answers a real phone call on smiths-net."** It goes
at the top of the README and into every launch post. Everything else is
secondary to this.

Goal: **≤ 60 s**, wow within the first **10 s**, loop-friendly, readable
on a phone screen.

Output targets:
- `docs/assets/demo.gif` — muted, ~760 px wide, for the README (GIF
  auto-plays and loops on GitHub; keep it under ~8 MB).
- `demo.mp4` (with audio) — the real hero for Reddit/HN/X/YouTube. The
  audio *is* the story; keep it.

---

## Shot list (target ~45 s)

| t (s) | Shot | On screen | Why |
|------:|------|-----------|-----|
| 0–4  | **Cold open** | A phone dialing a real number. Caption: *"Calling a phone number…"* | Instantly concrete — this is a *phone call*, not a chatbot. |
| 4–10 | **Pickup** | Split view: phone at ear + terminal. Engine log shows `INVITE` → `200 OK`, bot greeting plays. | The "wow": a computer answered a phone. |
| 10–30 | **Conversation** | You ask 2–3 real questions; the AI answers in natural speech. Overlay the live transcript as it appears. | Proves it's real, low-latency, and coherent. |
| 18–30 | **The proof overlay** | Corner badge: **"☁️ 0 bytes to cloud"** + a `nvtop`/`nvidia-smi` GPU meter pinned in a terminal split. | This is the differentiator vs every cloud voice bot. Hammer it. |
| 30–40 | **Barge-in** | Interrupt the bot mid-sentence; it stops and listens. | Shows real conversational UX, not turn-locked TTS. |
| 40–45 | **Hang-up + tag** | Bot says goodbye and drops the call itself (`BYE` in the log). End card: repo URL + one-liner. | Clean close; drives the click. |

Keep captions burned-in (many people watch muted first, then rewind with
sound). One idea per caption.

---

## Two ways to capture it

### A. Dramatic (recommended): real phone → trunk mode

Film a phone calling the DID while screen-recording the terminal. This is
the version that gets shared.

1. Bring up the offline stack + trunk bot (see
   [`examples/README-asr-bot.md`](../../examples/README-asr-bot.md)):
   ```bash
   cp examples/multifon.env.example examples/multifon.env   # trunk creds
   bash examples/setup-offline.sh          # once: models + llama.cpp + Gemma
   bash examples/start-offline-stack.sh    # llama-server (bg) + bot (fg)
   ```
2. In a second terminal, pin a GPU meter for the "it's all local" proof:
   ```bash
   nvtop            # or: watch -n0.5 nvidia-smi
   ```
3. Screen-record the two terminals; film the phone separately, then
   picture-in-picture the phone into a corner in edit.
4. Call the DID from a mobile and have the scripted conversation.

### B. Reproducible: local loopback (no trunk needed)

Anyone can reproduce this, and there are no carrier creds on screen.

```bash
# Terminal 1 — park the bot on the "voicebot" rendezvous key
bash examples/restart-asr-bot.sh          # uses examples/offline.env

# Terminal 2 — GPU proof
nvtop

# Terminal 3 — call the bot with a spoken utterance, capture its reply
python3 examples/python-client/voice_caller.py --out tmp/asr-bot-reply.wav
```

Screen-record all three terminals. Loopback loses the "phone in hand"
drama, so lean harder on the transcript overlay + GPU meter.

> Tip: whichever mode, do a silent dry run first and **script your 2–3
> questions**. Short, punchy, and in the assistant's configured language.

---

## Recording commands

### Screen capture

- **macOS:** QuickTime (File → New Screen Recording) is simplest, or
  ```bash
  # list devices, then capture screen index N with system audio
  ffmpeg -f avfoundation -list_devices true -i ""
  ffmpeg -f avfoundation -r 30 -i "N:0" demo-raw.mp4
  ```
- **Linux (X11):**
  ```bash
  ffmpeg -video_size 1920x1080 -framerate 30 -f x11grab -i :0.0 \
         -f pulse -i default demo-raw.mp4
  ```

### Terminal-only alternative (crisp text, tiny file)

If you go terminal-only (mode B), record with asciinema and render to GIF
with [`agg`](https://github.com/asciinema/agg) — text stays razor-sharp:

```bash
asciinema rec demo.cast --title "smiths-net: offline AI answers a call"
# …run the stack + the caller, then Ctrl-D…
agg --font-size 20 --theme monokai demo.cast docs/assets/demo.gif
```
(asciinema captures no audio — pair it with mode A's mp4 for the sound.)

### Edit → export

Trim, add burned-in captions and the corner overlays in any editor
(iMovie / DaVinci Resolve / CapCut). Export **two** files:

```bash
# 1) demo.mp4 — keep audio, this is the social/HN/Reddit hero
#    (export straight from your editor at 1080p)

# 2) docs/assets/demo.gif — muted, high-quality palette, README-sized
mkdir -p docs/assets
ffmpeg -i demo.mp4 -vf \
  "fps=15,scale=760:-1:flags=lanczos,split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse" \
  -loop 0 docs/assets/demo.gif

# If the GIF is > ~8 MB, drop fps to 12 or width to 640 and re-run.
```

Then swap the README's placeholder `<em>…</em>` line for the commented-out
`<img src="docs/assets/demo.gif">` block (it's right above it).

---

## Checklist before you post it

- [ ] First 10 s make it obvious this is a **live phone call answered by
      a local AI**.
- [ ] The **"0 bytes to cloud" / GPU meter** proof is on screen.
- [ ] Captions are burned in and readable muted.
- [ ] No secrets on screen (trunk creds, IPs, tokens, real DID).
- [ ] `demo.mp4` keeps audio; `docs/assets/demo.gif` is < ~8 MB and loops.
- [ ] End card shows `github.com/mindhalla-open/smiths-net`.
