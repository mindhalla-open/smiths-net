# Silero TTS (offline Russian)

Best open-source Russian TTS: auto-stress, homographs, questions (`v5_5_ru`).
Native **8 kHz** output — ideal for Megafon telephony without resampling.

## Install

```bash
pip install torch silero
```

The model (~150 MB) downloads on first synthesis and is cached locally.
After that, synthesis works with **no network**.

## Voices (`v5_5_ru`)

| Speaker  | Description        |
|----------|--------------------|
| `xenia`  | Female (default)   |
| `baya`   | Female, soft       |
| `kseniya`| Female             |
| `aidar`  | Male               |
| `eugene` | Male               |

## Environment

```bash
TTS_PLUGIN=ai-tts-silero
SILERO_MODEL=v5_5_ru      # also v5_4_ru, v5_3_ru, v5_ru
SILERO_SPEAKER=xenia
SILERO_THREADS=4
SILERO_DEVICE=cpu
```

## Alternative: Piper

Lighter weight, no PyTorch — good for embedded/RPi:

```bash
# Install piper binary + download voice:
# https://github.com/rhasspy/piper/blob/master/README.md
TTS_PLUGIN=ai-tts-piper
PIPER_VOICE=/path/to/ru_RU-irina-medium.onnx
PIPER_VOICE_ID=irina
```

Piper is slightly lower quality than Silero v5 but faster on weak CPUs.

## Comparison (2026)

| Engine  | Russian quality | Offline | Telephony 8 kHz | Deps        |
|---------|-----------------|---------|-----------------|-------------|
| Silero v5_5_ru | ★★★★★ | Yes     | Native          | torch, ~150 MB |
| Piper irina    | ★★★☆  | Yes     | Resampled       | piper binary |
| Edge TTS       | ★★★★  | No      | Resampled       | network      |
| SaluteSpeech   | ★★★★  | No      | Native          | network      |
