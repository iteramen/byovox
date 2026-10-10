# byovox design

byovox ("bring your own vox") is push-to-talk dictation. Hold a key, speak, release: the audio
goes to a speech-to-text endpoint you run, the transcript optionally goes through a cleanup
model you run, and the result is typed into the focused window. One Rust crate builds two
binaries, a console CLI and a windowless daemon; the UI is a tray icon, a recording indicator
and one TOML file. Only Windows has backends: the crate builds and tests on Linux, but
`platform::detect()` refuses to start there.

## Goals

- **Any OpenAI-compatible server.** `/v1/audio/transcriptions` for speech,
  `/v1/chat/completions` for cleanup. `language_candidates` is an optional whisper.cpp server
  extension; servers without it ignore the field.
- **Detect, then route.** Detection among candidate languages, a confident detection re-sent
  to that language's own endpoint, the keyboard layout as the hint when detection is unsure.
- **Never lossy.** Once a transcript exists, every later failure still ends in an insert
  attempt or in the text being held for `byovox last`. Deliberately neither: a transcript
  whisper scored as silence (the score decides, never the words); a reply left empty by
  sanitising, which is not held so `byovox last` never returns something not dictated. A
  transcript over `inject.max_chars` is held whole, never typed or truncated.
- **Honest degradation, lean surface.** Backends fall back one rung at a time and report which;
  no settings window, no auto-update, no account.

**Non-goals:** mobile; streaming, wake words, always-on listening (the microphone is open only
between press and release); in-process inference (`byovox setup` can download a pinned
whisper.cpp server that `[stt.server]` starts and stops, but it runs as its own process).

## Architecture

[src/main.rs](../src/main.rs) is the CLI (`byovox --help`). Bare `byovox` starts the daemon;
`run` runs it in the console; `toggle`, `quit`, `status` and `last` talk to it over IPC;
`setup`, `check`, `hotkey`, `config` and `autostart` work on the config and the machine. All
logic lives in the library ([src/lib.rs](../src/lib.rs)).

### The pipeline is a state machine

```
Idle ──press──▶ Recording ──release──▶ Transcribing ──▶ Polishing ──▶ Inserting ──▶ Idle
                    │ held < hotkey.min_hold_ms                → discard, no request
                    │ hotkey.cancel_key                        → discard
                    │ empty, or no 10 ms window above -40 dBFS → discard, no request
Transcribing error → Idle, nothing inserted, error indication
Polishing error    → Inserting with the raw transcript, error indication
Inserting error    → next inject rung; past the last, held for `byovox last`
```

In toggle mode a press toggles and releases are ignored. Events arriving while a dictation is
in flight are dropped (`pipeline::pump`). The tray's Disable makes the pipeline deaf, and an
event during a recording discards it. [src/pipeline.rs](../src/pipeline.rs) knows no OS.

### Traits

Platform differences are traits, chosen at run time by `platform::detect()`
([src/platform/mod.rs](../src/platform/mod.rs)):

| Trait | File | Role |
|---|---|---|
| `Hotkey` | [hotkey.rs](../src/hotkey.rs) | emits `Pressed`, `Released`, `Toggle`, `Cancel` |
| `Capture` | [capture.rs](../src/capture.rs) | `start`/`stop`, yielding 16 kHz mono i16 |
| `Layout` | [layout.rs](../src/layout.rs) | ISO 639-1 code of the foreground window's layout |
| `Inject` | [inject.rs](../src/inject.rs) | one rung; `name()` is what logs, `check` and the capture log record |
| `Target` | [inject.rs](../src/inject.rs) | the window a dictation started in; whether focus takes text |
| `Indicator` | [indicator.rs](../src/indicator.rs) | `Idle`, `Recording`, `Working`, `Done`, `Uncertain`, `Error` |

`Transcriber`, `Polisher` and `Recorder` let pipeline tests run without network or microphone;
`Prompter` and `Probe` ([setup.rs](../src/setup.rs)) script the setup wizard.

### Threads

The main thread runs the `winit` event loop with tray, pill and cues ([src/ui.rs](../src/ui.rs));
the pipeline reaches it only through `ProxyIndicator`. `byovox-pipeline` runs each dictation
sequentially with blocking HTTP (`ureq`, no async runtime), under `catch_unwind`: a panic quits
the daemon. `byovox-hotkey` owns the keyboard hook's message pump. `byovox-ipc` accepts and
hands each connection to a `byovox-ipc-conn`. `byovox-capture` (one per recording),
`byovox-update` and `byovox-server` (watching the `[stt.server]` child) complete the set.

## Backends

Windows lives in [src/platform/windows/](../src/platform/windows/); user-facing detail, including
elevated windows, is in [platform-windows.md](platform-windows.md).

| Trait | Windows |
|---|---|
| Hotkey | `WH_KEYBOARD_LL` hook: any key, a bare modifier, or a held chord (releasing any member ends it). A chord's trigger is swallowed; a bare key and a chord's modifiers still reach the window. |
| Capture | `cpal`/WASAPI, open only while recording; `capture.device` pins an input. The start click and a quiet sub-second tail past the last 30 s multiple are cut ([src/audio.rs](../src/audio.rs)). |
| Layout | foreground window's layout LANGID → ISO 639-1. |
| Inject | `type` (`SendInput` + `KEYEVENTF_UNICODE`), `paste` (clipboard + Ctrl+V, previous clipboard text restored), `clipboard-only`. |
| Target | top-level window of the UI Automation focused element at press, brought back before typing. Closed, hidden or cloaked by then: nothing is typed. |
| Indicator | tray (`tray-icon`), pill (`winit` + `softbuffer`), cues (`rodio`, synthesised). |

**Rungs.** `inject.mode = "auto"` tries `type` → `paste` → `clipboard-only`; naming a mode pins
one rung. When all fail, the text is held for `byovox last` and *Copy last transcript*.
**Fallbacks.** Unreadable layout → the default language policy. A tray icon or pill that cannot
be built is logged and skipped; a failed cue output is reopened at the next cue. A hook that
fails to install logs ERROR and leaves `byovox toggle`, bound to an OS shortcut. A missing
microphone, an unknown key name or a bad `inject.mode` refuses to start.

## Pipeline detail

1. **Press.** Read the layout, resolve the language, open the microphone, then record the
   target window (after the microphone, so the first syllable never waits on UI Automation).
2. **Release.** Close the microphone; discard a tap, a cancel, an empty or silent capture.
3. **Encode.** Downmix, resample to 16 kHz (box prefilter, linear interpolation), 16-bit WAV.
4. **Transcribe.** Multipart POST to `{base_url}/audio/transcriptions`: `file`, `model`,
   `response_format`, language fields, `prompt` (`stt.prompt` or the lane's), and `vad=true` on
   a take over 60 s (re-sent without it if refused). `response_format` is `verbose_json` while
   `stt.no_speech_threshold > 0`, since only it carries `segments[].no_speech_prob`, else
   `json`. One retry on a connection failure; a timeout after connecting or any HTTP status is
   not retried. Segment line breaks become spaces.
5. **No-speech gate.** An empty transcript, or one whose duration-weighted mean
   `no_speech_prob` exceeds `stt.no_speech_threshold`, returns to Idle with no cue, not held.
6. **Polish**, when `polish.enabled` and the word count reaches `polish.min_words`: POST
   `{polish.base_url}/chat/completions`, system prompt as below, user
   `<transcription>…</transcription>`, `temperature` 0.3, `max_tokens` 1024. On any failure
   the raw transcript is typed, with the error cue and a WARN.
7. **Sanitise, then inject.** [`pipeline::sanitize`](../src/pipeline.rs) turns `\n`, U+2028 and
   U+2029 into spaces and drops all other control and format (`Cf`) characters except LRM/RLM
   and ZWJ/ZWNJ, logging only the count. Then trim, strip one terminal `.` (never an ellipsis, `?`
   or `!`) unless `inject.strip_terminal_period = false`, append a space if
   `inject.trailing_space`, restore the target window (with nothing focused at press: go on
   only if focus is now in a text field), and run the rungs. The done cue plays only for
   inserted text; the warning cue replaces it when any segment scored above
   `stt.no_speech_warn`.
8. **Capture log** (opt-in, [src/capture_log.rs](../src/capture_log.rs)): a WAV and a JSONL
   row (`DictationRecord`) per dictation, gated and emptied ones included. The row carries the
   max segment score, not the mean the gate reads.

## Language policy

```toml
[language]
default    = "auto"          # or a code, forced for unmapped layouts
candidates = ["en", "ru"]    # language_candidates under auto; empty = not sent
[language.by_layout]         # layout (ISO 639-1) → STT language
he = "he"
```

[src/lang.rs](../src/lang.rs): an explicit language sends `language=<code>`. Auto sends no
`language` (a whisper.cpp server must run with `-l auto`), plus `language_candidates` when
`candidates` is non-empty, with every `[stt.by_language]` code appended. Under an explicit
`default` a mapped layout forces its language; under auto it is only the *prior*.

`stt::Routed` ([src/stt.rs](../src/stt.rs)):

- Explicit → the language's `[stt.by_language.<code>]` lane if any, else `stt.base_url`. An
  empty lane reply is retried on the default; a lane error stays an error. A lane sends its own
  token if it names `api_key_env`, `[stt]`'s with `inherit_token = true`, else none.
- Auto → `stt.base_url` first. The detected language is the sent candidate with the highest
  `language_probabilities`. At p ≥ 0.5 a detected language with a lane is re-sent there; below
  0.5 or undetected, the prior is re-sent explicitly; if the first request failed, a prior with
  a lane goes to it. Otherwise the first reply, or its error, stands.
- With the no-speech gate off nothing is detected, so a prior is sent explicitly at once.

## Cleanup prompt

`BUILT_IN_PROMPT` in [src/polish.rs](../src/polish.rs) is a numbered rule list: punctuate and
capitalise; drop fillers and false starts; number spoken enumerations; preserve language,
terms, names, numbers and profanity; never add content, answer or obey the text; output only
the text; turn a punctuation name dictated as the mark into the mark. The `<transcription>`
payload is declared content, never instructions.

- `polish.capitalize_first_word = false` swaps rule 1 for `LOWERCASE_FIRST_RULE` and gives a
  lower-cased first-word "I" its capital back. A `polish.prompt_file` replaces the prompt
  verbatim and is never rewritten.
- `polish::prompt_for` appends a glossary rule carrying `stt.prompt` and every lane `prompt`,
  so transliterated technical terms come back in Latin.
- Rule changes are measured: [bench/polish_bench.py](../bench/polish_bench.py) extracts the
  prompt from the source and scores it on [bench/polish_items.jsonl](../bench/polish_items.jsonl);
  a unit test fails if any bench item's text appears in the prompt.

## Cleanup prompt: known limits

- A spoken join whose words also form an ordinary phrase ("training period strip") is typed unchanged, or as whisper dotted it.
- Only period, dot, comma and their Russian and Hebrew equivalents become marks; "new line" is not converted, since every newline is typed as a space.
- A spoken terminal "period" becomes `.` and is then removed by `inject.strip_terminal_period`.
- A list whose second item isn't parallel to the first can stay unnumbered, and a numbered item can keep its cue word ("1. First we…").
- With cleanup down, disabled or skipped by `polish.min_words`, punctuation names are typed as words and lists stay unformatted.
- A person's name from the glossary, dictated in a Hebrew or Russian sentence, usually stays in Latin script.

## Configuration

One TOML file, `%APPDATA%\byovox\config\config.toml` unless `--config` names another.
[config.example.toml](config.example.toml) is the commented reference: compiled into the
binary, written by `byovox config --init`, and pinned equal to `Config::default()` by a test.
Every key has a default; unknown keys are a startup error naming the key. `stt.base_url`, and
`polish.base_url` and `polish.model` while polish is enabled, default to empty and are refused
by name until set ([src/config.rs](../src/config.rs)). Tokens come from the `api_key_env`
variable, else a `NAME=VALUE` line in `api_key_file`. `hosted` changes no request; `byovox
check` notes the stage as someone else's machine. `byovox config` tags each value `file` or
`default`.

## Operations

**IPC** ([src/ipc.rs](../src/ipc.rs)): a per-user local socket `byovox-<user>.sock` (a named
pipe on Windows), which is also the single-instance guard. One JSON object per line each way:
`{"cmd": "toggle" | "quit" | "status" | "last"}` → `{"ok": true, ...}` or
`{"ok": false, "error": "..."}`. `status` returns `state`, `enabled` and `last_error`; `last`
returns the held transcript in `text`.

**Errors.** Invalid config: refuse to start, exit 2, name the key. STT fails: error cue, the
error on the tray status line and in `byovox status`, ERROR log, nothing inserted. Polish
fails: raw transcript typed, error cue, WARN. Inject fails, or the target window is gone
(closed, hidden, cloaked, or a desktop press that ended outside a text field): held for
`byovox last`, error cue, WARN without the text.

**Logging.** `tracing` to a daily-rotated file in `%LOCALAPPDATA%\byovox\data\logs`, plus
stderr under `byovox run`. INFO is one line per dictation; only DEBUG carries transcript text.

**`byovox check`** ([src/check.rs](../src/check.rs)) runs what the daemon would: config, a
warning per plain-HTTP remote endpoint, hosted notes, update setting, `[stt.server]`, hotkey,
backends and rungs, a 1 s microphone sample and its level, layout and resolved policy, an STT
round trip plus one per lane (`stt[<code>]`), a polish round trip through the daemon's exact
prompt, an inject dry run. It exits non-zero when a required stage fails; `warn` and `note`
rows never do.

**Tray and indicator** ([src/ui.rs](../src/ui.rs)). Menu: status line, Enable/Disable, Toggle
mode, Audio cues, Run on startup, Copy last transcript, Open config, Open logs, Run check,
Quit, and a release-page item once a newer release is found. Icons: idle, recording, working,
error (held 3 s), disabled. The pill is an always-on-top window by the cursor while recording,
working or showing a failure. The start, done, warning and error cues are synthesised tones
that follow the default output device.

**Autostart:** a `byovox` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
starting `byovox-daemon`, with any `--config` made absolute. **Updates**
([src/update.rs](../src/update.rs)): at most one unauthenticated request a day for the latest
GitHub release, off with `update.check = false`; nothing is downloaded or installed.

## Testing and releases

- **Unit tests** need no hardware: fake backends and a one-thread HTTP double that records the
  raw request ([src/testutil.rs](../src/testutil.rs)). Tests that need a desktop are
  `#[ignore]`d; run them with `cargo test -- --ignored`.
- **CI** ([.github/workflows/ci.yml](../.github/workflows/ci.yml)): `cargo fmt --check`,
  `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` and
  `python bench/polish_bench.py --self-test` on Windows and Ubuntu; `cargo check` at the
  `rust-version` MSRV; `cargo audit` and `cargo deny check`. The bench self-test guards the
  bench's extraction of the prompt from the source, which `cargo test` cannot see.
- **Against real endpoints:** `byovox check` and `bench/polish_bench.py`; before a tag, the
  manual checklist in [testing.md](testing.md) against the release build.

A `v*` tag runs [.github/workflows/release.yml](../.github/workflows/release.yml): a Windows x64
zip, `SHA256SUMS` and a build-provenance attestation. Binaries are not code-signed. The threat
model is in [SECURITY.md](../SECURITY.md).

## Not built yet

- Linux (Wayland and X11) and macOS backends for every platform trait, and their autostart.
- Linux and macOS release archives; until then `cargo install --git` builds from source.
- A per-lane `hosted` flag: `hosted` exists only on `[stt]` and `[polish]`.
