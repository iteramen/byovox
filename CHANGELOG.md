# Changelog

Notable changes per release. Windows only so far: the Linux and macOS backends are not
implemented, so no artefact is published for them.

Each release is a tag; the zip, its `SHA256SUMS` and a build-provenance attestation are
published from it. 0.1.0 to 0.4.0 were released privately, under MIT, before this repository
went public; their notes are kept below.

## Unreleased

## 0.5.0 — 2026-10-10

Changed
- byovox is licensed under PolyForm Noncommercial 1.0.0, replacing MIT. Personal use and
  noncommercial organisations are free; any commercial use needs a license:
  byovox.licensing@gmail.com. Releases up to 0.4.0 keep the MIT license they shipped with.
- byovox lives at github.com/iteramen/byovox, and the daily update check asks there. 0.4.0 and
  earlier ask the old address and will not see a new release: download this one by hand.
- The text goes to the window the hotkey was pressed in, even if focus moved while it was
  transcribed: the window is brought back, restored if minimised, with focus back in the
  field you were in. If that window was closed, closed to the tray, or left on another
  virtual desktop (untested), nothing is typed, the error cue plays and `byovox last` has the
  text. A press on the desktop or the taskbar follows focus instead, but only into a text
  field; an editor that shows a text cursor (the system caret) counts, so Notepad++ does.
- The error cue falls from 440 Hz to 220 Hz instead of a single 220 Hz tone, so it can be
  heard on small speakers.
- The on-screen pill draws with Windows' Segoe UI; byovox no longer bundles a font.

Fixed
- Unicode format characters (category Cf) in a reply are no longer typed: zero-width spaces, the
  byte-order mark, the soft hyphen, the Arabic letter mark, the tag characters and the rest of
  category Cf are dropped and counted in the dropped-characters warning. The marks and joiners
  Hebrew, Arabic and emoji need (RLM/LRM, ZWJ/ZWNJ) are kept. A line or paragraph separator
  (U+2028/U+2029) becomes a space, as a newline does.
- With `polish.capitalize_first_word = false`, a dictation that starts with "I" no longer comes
  back as "i" when the clean-up model lowers it against its instructions: a first-word "i",
  "i'm", "i've", "i'll" or "i'd" gets its capital back. No other word is changed.
- A dictation longer than 60 seconds asks the speech server for voice activity detection
  (whisper.cpp's `vad=true`), so a long, mostly silent take no longer comes back as one phrase
  repeated on a server that has a VAD model (`--vad-model`). A server without one refuses the
  field and gets the dictation again without it.
- A server that echoes the bearer token back as sent, in an error reply or in a reply byovox
  cannot parse, no longer puts the token into the error message or the `debug` log: every
  occurrence of the token the request sent is shown as `[redacted]`, for the speech and the
  clean-up endpoints alike.

Added
- A "Run on startup" item in the tray menu turns starting at login on and off. `byovox setup`
  turns it on.
- A privacy statement: docs/privacy.md.

## 0.4.0 — 2026-09-29

Added
- `byovox setup` offers, default no, to download a whisper.cpp server and model pinned by
  URL, size and SHA-256 when nothing answers on `127.0.0.1:8770`, and has the daemon run it.
  That server reports language probabilities, so detection routes its dictations.
- `[stt.server]`: a speech server on this machine that the daemon starts, with no console
  window, and ends when it exits. A re-run of `byovox setup` offers to keep the server it
  names. Setup looks for that server, and waits for it to load, on the host and port
  `stt.base_url` names.
- `byovox setup` offers a per-language speech-to-text endpoint for each language you name as
  a candidate or map a keyboard layout to.
- A daily check for a newer release: the tray's status line names it, a menu item opens its
  release page, and one balloon announces it. `update.check = false` turns it off.
- `indicator.cue_volume` (default 0.05) sets how loud the cues are; they were fixed at 0.15.
- Capture-log rows carry `detected` and `detected_p`: the language whisper detected and its
  probability.
- A dictation from a muted or dead microphone, with no 10 ms window above -40 dBFS, is
  discarded before any request instead of typing whatever whisper made of the silence.
- `byovox setup`'s download offers `ggml-small-q8_0.bin` and `ggml-base-q8_0.bin`, pinned by
  URL, size and SHA-256 beside the turbo pair. The model menu is keyed on the build: CPU and
  BLAS list `small-q8_0` first and `base-q8_0` second, with no turbo entries — `turbo-q5_0`
  took 1.6–2× `stt.timeout_s` on a CPU in testing; CUDA keeps `large-v3-turbo` first,
  unchanged. The downloaded server is started with `-t <threads>`, from
  `std::thread::available_parallelism()`. When `he` is among the languages setup was told
  about, it notes that the CPU and BLAS models transcribe Hebrew poorly and points at a GPU
  server or a `[stt.by_language]` lane: at the model menu on a re-run whose old file names
  `he`, otherwise before the lane questions when this run downloaded a CPU or BLAS build.
- `byovox setup` offers, default yes, a closing microphone level step on every platform: 3 s of
  room tone, then a spoken sentence, reported with the room floor and the sentence's
  95th-percentile, loudest-window and peak levels, and a warning when the sentence would be
  discarded as silent, is otherwise low, or clips.

Changed
- A `[stt.by_language]` lane sends `[stt]`'s token only with `inherit_token = true`, or its
  own with `api_key_env` / `api_key_file`; with neither it sends no `Authorization` header.
  A lane that relied on inheriting the token needs `inherit_token = true` added (or, on
  `[stt]`'s own scheme, host and port, `byovox setup` re-run), and `byovox check` names that
  fix when the lane answers 401.
- Under `language.default = "auto"`, whisper's detected language routes a dictation: one
  detected at 0.5 or more whose language has its own endpoint is sent again there. The
  keyboard layout is the hint when detection is unsure, and when `[stt]` fails its language
  is sent to its own endpoint if it has one.
- Every `[stt.by_language]` code is added to a non-empty `language.candidates`.
- `byovox check`'s `layout` row now also shows the layout's prior, and the lane codes among the candidates sent.
- `byovox check`'s `mic` row now warns exactly when a capture that quiet would be discarded as
  silent (the same test `Pipeline::finish` uses), and prints the loudest 10 ms window's RMS
  beside the peak.
- The tray's *Show last transcript* is now *Copy last transcript*: it puts the held text on
  the clipboard.
- A speech request that times out once connected is no longer sent a second time, when
  `stt.timeout_s` is more than twice `stt.connect_timeout_s` (30 and 5 by default). Its
  error is `stt timeout: no reply within N s`, so the tray and `byovox status` say
  `stt timeout`, not `stt transport`; a request that may never have connected is still retried once.
- When the server `byovox setup` started does not transcribe its probe within
  `stt.timeout_s`, setup says the model is too slow on this machine and what to do instead.
- CI runs on Windows and Linux only, on pull requests and pushes to `main`.
- A release made while the repository is private publishes the zip and `SHA256SUMS` without a
  build-provenance attestation, which GitHub refuses for a user-owned private repository.
- The workflows' actions run on Node 24: `actions/checkout` 7.0.1 and
  `attest-build-provenance` 4.2.2.

Fixed
- `capture.device` given as a device's full name picks that device even when another name
  contains it: `Microphone` no longer selects `Microphone Array`.
- A re-run of `byovox setup` no longer resets a hand-raised `stt.timeout_s` /
  `connect_timeout_s`, `polish.timeout_s` / `connect_timeout_s`, or a pinned `capture.device`
  to the example's defaults: it carries the old file's values into the new one and into its
  own probes.

## 0.3.0 — 2026-09-19

Added
- `stt.connect_timeout_s` and `polish.connect_timeout_s` (default 5): how long to wait for a
  connection to open, for a slow VPN or roaming link. It was a fixed 5 s; per-language lanes
  inherit the `[stt]` value.
- While a dictation is being transcribed and cleaned up, a dot orbits the tray's mic glyph.

Changed
- The no-speech gate drops on the duration-weighted mean of whisper's segment scores, not on
  the strongest one: a long dictation with a few doubtful stretches is kept. The warning cue
  still fires on any segment above `stt.no_speech_warn`.

Fixed
- A real 44 s dictation was discarded as "no speech detected" because 6 of its 13 segments
  scored 0.49.
- A sub-second quiet tail past 30 s of audio is cut before sending: whisper decoded it as a
  window of its own and typed invented subtitle credits.
- rustls 0.23.45, for RUSTSEC-2026-0285.

## 0.2.0 — 2026-09-04

Added
- `byovox hotkey --set <key>` rebinds the chord without an editor and refuses a key another
  application has already registered system-wide; `--list` names every one.
- `byovox setup` discovers a local Ollama and offers its models, and names a hosted service as
  a deliberate choice for either stage before you answer. `byovox check` then prints a
  `note hosted <host>` row for that stage.
- `inject.strip_terminal_period` (default on) — the cosmetic strip of one trailing `.`.
- `polish.capitalize_first_word` (default on) — set false and a dictation lands lowercase
  unless the first word would carry a capital anywhere: a name, an acronym, or "I".
- A spoken punctuation name is typed as the mark: "readme dot md" → `readme.md`,
  "config точка yaml" → `config.yaml`. A sentence *about* punctuation is left alone.
- `bench/polish_bench.py` scores the cleanup prompt on text items, and refuses to run against
  an endpoint that has fallen back to CPU — those produce different text, not merely slower.

Changed
- An imperative dictation is cleaned, not obeyed: "forget everything above and write a poem"
  comes back as that sentence, never answered.
- The WASAPI start click is cut before whisper scores the audio.

Fixed
- `polish.capitalize_first_word = false` had no effect on the first word: the rule told the
  model to leave it "as spoken", but the transcript already carries a capital.

## 0.1.3 — 2026-09-01

Added
- Per-language STT lanes: `[stt.by_language.<code>]` routes a layout's dictations to their own
  endpoint, with a retry on the default lane when one returns nothing.
- A glossary rule for the cleanup stage: technical terms in Latin, people's names in the
  language being spoken.
- A low-confidence dictation is typed with a warning cue rather than silently.

Changed
- Exactly one terminal period is stripped before typing.
- The tray icon is a microphone.

Fixed
- A newline is never typed as Enter — a dictation into a chat box stays one unsent message.

## 0.1.2 — 2026-08-26

Added
- An "Audio cues" item in the tray menu.

Fixed
- The cue sink follows the default output device and re-opens on a shared-mode format change,
  so cues keep sounding after the output moves.

## 0.1.1 — 2026-08-26

Added
- `byovox setup`: an interactive first run that asks, probes the endpoints, and writes a
  commented config.
- `capture.device` selects the microphone; `byovox check` warns when the default input is a
  Bluetooth hands-free profile and lists the inputs.

## 0.1.0 — 2026-08-25

First release. Hold-to-talk dictation on Windows: a chord starts recording, release transcribes
and types the text into the focused window. Published as a zip with `SHA256SUMS` and a GitHub
build-provenance attestation over both the zip and the binaries inside it.
