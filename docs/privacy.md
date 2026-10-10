# Privacy policy — byovox

**Effective: 2026-10-10.**

byovox is a Windows push-to-talk dictation tool. You hold a hotkey, speak, release, and the
transcript is typed into the window you were in. byovox runs no server of its own and has no
account, no telemetry and no analytics: it sends your speech to servers you choose, either ones
you run yourself or a hosted service you pick. This policy describes what the program does. Each
statement names the source file and symbol that does it, in the
[byovox repository](https://github.com/iteramen/byovox), so it can be checked.

## The microphone

byovox opens the microphone when you press the hotkey and closes it when you release it, or, in
toggle mode, on your second press (`src/pipeline.rs` — `Pipeline::handle`, `begin`, `finish`;
`src/capture.rs` — `CpalCapture::start`, `stop`). Pressing the cancel key (Escape by default), or
choosing *Disable* in the tray during a recording, closes it and discards the audio unsent. There
is no time limit on a recording.

Audio is held in memory and never written to disk, unless you turn on the
[capture log](#what-stays-on-the-computer). A press shorter than `hotkey.min_hold_ms` (250 ms by
default), an empty recording and a silent one are discarded without any request
(`Pipeline::finish`). The audio is dropped when the dictation ends.

Outside dictation, the microphone is opened only by commands you run: `byovox check` records
1 second (`src/check.rs` — `sample_microphone`), and `byovox setup`'s level check records 3 and
5 seconds, which are measured locally and discarded (`src/setup.rs` — `mic_step`).

## Where the audio goes

byovox ships with no server address: `stt.base_url` and `polish.base_url` are empty until you set
them (`src/config.rs` — `SttConfig`, `PolishConfig`). Once you do:

- **Your speech server.** The recording is sent as a WAV file to
  `POST {stt.base_url}/audio/transcriptions`, once, or twice to that same address when no response
  arrives at all (`src/stt.rs` — `SttClient::send`). With it go the model name, the response
  format, your glossary (`stt.prompt`) if you set one, and either `language` (a two-letter code)
  or, on auto-detect, the `language_candidates` you listed, if any (`src/lang.rs` —
  `SttLanguage::form_fields`, `LanguagePolicy::resolve`). The code can come from the keyboard
  layout of the window you dictate into, mapped by `language.by_layout`
  (`src/platform/windows/layout.rs` — `WinLayout::current`); only the code is sent. A recording
  longer than 60 seconds
  carries `vad=true`; if the server answers that with an HTTP error, the recording is sent once
  more without it (`SttClient::transcribe`, `is_long_take`).
- **Language lanes.** If you set a separate server for a language under `[stt.by_language]`, a
  dictation sent with that language goes there, with that lane's glossary if it has one; if the
  lane answers with no text, the same recording goes to your default speech server. On
  auto-detect with the no-speech check on (the default), the recording goes to the default server
  first and may be sent once more: to the lane of a language the reply detects with probability
  0.5 or more; otherwise, when `language.by_layout` maps the keyboard layout, with that language,
  to its lane or else to the default server; and to that lane if the default fails. With the check
  off, a mapped layout's language goes straight to its lane (`src/stt.rs` — `Routed`,
  `ROUTE_P`). Every server here is one you configured.
- **The cleanup server (optional).** If `polish.enabled` is on (the default; byovox refuses to
  start with it on and no address or model set), the transcript text, not the audio, is sent as
  one request to `POST {polish.base_url}/chat/completions`, with the cleanup instructions and the
  glossaries of `[stt]` and every lane (`src/polish.rs` — `PolishClient::polish`, `prompt_for`).
  It is not sent for an empty transcript, one the no-speech check dropped, or one shorter than
  `polish.min_words` (`Pipeline::finish`). It is never retried.
- **Bearer tokens.** A token travels only as an `Authorization: Bearer` header
  (`SttClient::post_once`, `PolishClient::polish`). byovox reads it at startup from the
  environment variable named in `api_key_env`, or else from a `NAME=VALUE` line in the file named
  in `api_key_file` (`src/config.rs` — `resolve_token`). The speech token goes to your speech
  server and the cleanup token to the cleanup server. A lane gets its own token if it names an
  `api_key_env`, the speech token if it sets `inherit_token = true`, and otherwise none
  (`SttConfig::lane_config`).
- **`byovox check`.** It sends its 1-second recording to your speech server and to every lane,
  and the fixed sentence `um so this is uh a test` to the cleanup server when that is enabled, and
  prints the first 60 characters of each reply on the console (`src/check.rs` — `run`,
  `stt_round_trip`, `polish_round_trip`). The tray's *Run check* runs the same command.
- **`byovox setup`.** It tests the servers you name with one second of digital silence and the
  same fixed sentence (`check::probe_stt`, `probe_polish`), and ends by running `byovox check`.
  It asks a local Ollama at `http://127.0.0.1:11434/api/tags` which models it has
  (`src/ollama.rs` — `discover`). To find a running server it opens a TCP connection, sending
  nothing, to `127.0.0.1:8770` and to your speech server's host and port, and while a server it
  started is loading it asks that server's `/health` (`src/deploy.rs` — `port_in_use`,
  `wait_healthy`).

When you pick a hosted service, the setup wizard first tells you that your audio, or every
dictation's text, will be sent to someone else's machine, and records your choice as
`stt.hosted` or `polish.hosted` (`src/setup.rs` — `hosted_notice`, `ask_stt`, `ask_polish`);
Ollama models served from ollama.com are marked the same way. `byovox check` then prints a
`note hosted` row naming each such server (`check::note_hosted`). The flag changes no request.

The servers you configure keep or delete what they receive under their own policies, which byovox
neither sets nor sees; check the policy of any server you did not set up yourself.

## Other network requests

Apart from the servers you configure, byovox makes two kinds of request:

- **The update check.** When the daemon starts, and only if the last check was 24 hours or more
  ago, it sends one unauthenticated `GET https://api.github.com/repos/iteramen/byovox/releases/latest`
  (`src/update.rs` — `LATEST_URL`, `check`, `fetch`; `src/daemon.rs` — `start`). The request
  carries a `User-Agent: byovox/<version>` header and the standard `Host`, `Accept` and
  `Accept-Encoding` headers, and nothing about you, your configuration or your dictations. GitHub
  sees your IP address and byovox version. A newer release is shown in the tray; nothing is
  downloaded or installed, and its release page opens in your browser only if you click the tray
  item (`src/ui.rs` — `MenuAction::OpenRelease`). `update.check = false` turns the check off
  (`update::spawn`).
- **The whisper.cpp download.** If nothing answers on `127.0.0.1:8770`, `byovox setup` offers,
  default no, to download a whisper.cpp server build and a Whisper model. Only after a yes, it
  fetches them over HTTPS only from `github.com` (the build) and `huggingface.co` (the model),
  following their redirects to those services' download hosts, and checks each against the size
  and SHA-256 pinned in byovox (`src/setup.rs` — `offer_download`; `src/deploy.rs` — `PINS`,
  `agent`, `fetch`). The server it sets up listens on 127.0.0.1 only (`deploy::server_args`).

byovox contacts no other address. `byovox last`, `status`, `toggle` and `quit` talk to the
running daemon over a named pipe on your computer (`src/ipc.rs` — `socket_name`, `serve`).

## What stays on the computer

- **The config file**, `%APPDATA%\byovox\config\config.toml` (`src/config.rs` — `default_path`):
  server addresses, model names, glossaries, language, hotkey and other settings. It holds the
  *name* of the environment variable and the *path* of the key file, never a token; `byovox setup`
  refuses a value that looks like a token where a variable name belongs (`src/setup.rs` —
  `looks_like_a_secret`). The key file is yours: byovox only reads it.
- **Logs**, `%LOCALAPPDATA%\byovox\data\logs`: one file a day, the seven newest kept
  (`src/daemon.rs` — `log_dir`, `init_logging`). See [Logs](#logs).
- **The update stamp**, `%APPDATA%\byovox\data\update.json`: the time of the last check and the
  last version announced (`src/update.rs` — `Stamp`, `stamp_path`).
- **The capture log**, only if you set `capture_log.enabled = true`; it is off by default. It
  writes a WAV of each dictation and a row in `dictations.jsonl` with the raw and cleaned-up text,
  the keyboard layout, the language sent and detected, the no-speech score, the cleanup model, the
  insertion method, the count of removed characters and the timings, to
  `%APPDATA%\byovox\data\capture` or `capture_log.dir` (`src/capture_log.rs` —
  `CaptureLog::record`). A row is written for every dictation the speech server returned text
  for, including one dropped as silence. Entries older than `capture_log.keep_days` (default 30)
  are deleted when the daemon starts and after each dictation; `0` keeps them for ever
  (`CaptureLog::prune`).
- **The whisper.cpp download**, if you accepted it: `%LOCALAPPDATA%\byovox\data\whisper-b5130`
  (`src/deploy.rs` — `root`).
- **Autostart**, if you turn it on: one `byovox` value under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` holding the path of `byovox-daemon` and of
  any `--config` you gave (`src/platform/windows/autostart.rs` — `enable`).
- **The last transcript**, in memory only, until the next dictation replaces it or the daemon
  stops (`src/pipeline.rs` — `Shared::last_transcript`). `byovox last` prints it, and the tray's
  *Copy last transcript* puts it on the clipboard when you click it.

byovox has no installer. Deleting `%APPDATA%\byovox` and `%LOCALAPPDATA%\byovox`, and running
`byovox autostart --disable`, removes everything it stored, except a `capture_log.dir` you set
elsewhere.

## What is not collected

- No recording is kept unless you turn on the capture log.
- No transcript is written to disk except by the capture log, or to a log at the debug level
  described under [Logs](#logs).
- No keystroke other than the hotkey is acted on, and none is recorded (below).
- byovox has no account, telemetry, analytics, advertising or crash-reporting code.

## The keyboard hook

To see the hotkey, byovox installs a Windows low-level keyboard hook, which receives every key
event in your session while the daemon runs (`src/platform/windows/hotkey.rs` — `hook_proc`). For
each event it compares the key code with the hotkey's keys and the cancel key, and checks whether
the event is one byovox typed itself. It swallows only the hotkey's trigger key; every other key,
including the cancel key, passes on to your window. No key is logged, stored or sent; the hook
keeps only whether the hotkey's own keys are held (`src/hotkey.rs` — `ChordTracker`).

## The window you dictate into

When you press the hotkey, byovox notes the foreground window, its window class (to recognise the
desktop and taskbar), the focused UI Automation element and the keyboard layout
(`src/platform/windows/inject.rs` — `WinTarget::current`; `src/platform/windows/layout.rs` —
`WinLayout::current`). When the text arrives it puts focus back there and asks only whether the
focused element accepts text and whether a text cursor is shown (`focus_accepts_text`,
`caret_shown`). byovox reads no text from any window, and it does not detect password fields: the
text goes into whatever field had focus.

## The clipboard

In the default `auto` mode byovox types the text as keystrokes, which leaves the clipboard alone,
and uses the clipboard only if typing fails (`src/platform/mod.rs` — `detect`;
`src/platform/windows/inject.rs`):

- **`paste`** saves the clipboard's text, puts the transcript there, presses Ctrl+V and puts the
  saved text back 150 ms later (`PasteInject`). If the clipboard held something other than text
  (an image, files) or nothing, it is not saved: it is replaced, and the transcript stays.
- **`clipboard-only`** puts the transcript on the clipboard and leaves it there
  (`ClipboardOnlyInject`).

Windows clipboard history, and cloud clipboard sync if you turned it on, can keep what byovox puts
on the clipboard; byovox does not opt out of them.

## Network security

byovox sends to `https://` and `http://` addresses alike. HTTPS certificates are checked against
the Mozilla root store bundled with byovox, with no switch to turn that off. Over plain `http://`
your audio, transcript and bearer token cross the network unencrypted; for any such address other
than this computer, `byovox check` prints a warning row and the daemon logs one warning at start
(`src/check.rs` — `warn_cleartext`; `src/config.rs` — `is_cleartext_remote`,
`CLEARTEXT_WARNING`). Use `https://`, or a private network such as Tailscale or WireGuard.

## Logs

At the default level, `info`, byovox logs no transcript and no token. A dictation's lines carry
timings, the language code, the insertion method, and counts such as removed characters or the
length of an over-long transcript, never the text (`src/pipeline.rs` — `Pipeline::finish`). A
failed request is logged by the head of its error only, never a server's reply
(`pipeline::summary`); a network error can name the server's host (`SttClient::send`). A lane is
named by its config key, not its address (`src/daemon.rs` —
`start`). A crash logs where it happened, not its message (`daemon::panic_location`).

At `debug` (and `trace`), the log also holds the raw and cleaned-up transcript of every dictation
and the full error messages, which can include a server's hostname and the first 200 characters
of a reply it could not use. The request's bearer token is replaced with `[redacted]` in those
(`src/stt.rs` — `redact`, `body_prefix`); a token the server re-encodes before echoing it is not
recognised. `byovox run` prints the same lines to the console.

## Children

byovox is not directed at children and keeps nothing about who is using it.

## Changes to this policy

Changes to this policy are announced in the repository's
[CHANGELOG](https://github.com/iteramen/byovox/blob/main/CHANGELOG.md), with a new effective date
here.

## Contact

Questions about this policy or your data: byovox.licensing@gmail.com. Non-sensitive questions are
also welcome as [GitHub issues](https://github.com/iteramen/byovox/issues).
