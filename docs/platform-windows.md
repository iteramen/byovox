# Windows

What Windows does differently, and the two things about it that surprise people.

## The hotkey

byovox installs a low-level keyboard hook (`WH_KEYBOARD_LL`) rather than registering a hotkey
with the shell, which is why a **bare modifier** works as a push-to-talk key: `ControlRight`
(the default), `AltRight`, `CapsLock`, `ScrollLock`, `Pause`, `Insert`, `F13`–`F24`. Every
accepted `hotkey.key` name is listed in [`config.example.toml`](config.example.toml); anything
else is refused at startup and by `byovox check`, which prints the full list.

The same hook is what makes a chord — `hotkey.key = "ControlLeft+ShiftLeft+Z"` — work, and it
**swallows the trigger**: pressed on top of its modifiers the `Z` reaches byovox and stops
there, so the editor underneath neither types a `z` nor sees a Ctrl+Shift+Z of its own.
Pressed without them it types as usual. The names are **virtual keys**, which the active
layout assigns — not physical positions. A non-Latin layout (Cyrillic, Hebrew, Greek) keeps
the US assignments for the alphanumeric block, so `Z` stays the key a US layout labels Z even
while it types я or ז. Another *Latin* layout follows its own printed letters instead: on
AZERTY `A` is the key US labels Q, and on QWERTZ `Z` is the key US labels Y — under those, a
chord key moves with the layout.

A **disabled** daemon swallows nothing: the tray's Disable disarms the hook as well as the
pipeline, so the chord's trigger types as usual until Enable puts it back. A recording in
progress when Disable lands is cancelled at once; only that chord's repeats and release stay
swallowed, or the window would get a key-up whose key-down it never saw.

The modifiers themselves pass through untouched — an app left holding a Shift that never comes
up is worse than any hotkey — so with the trigger swallowed the app sees them go down and come
back up with no key pressed in between, and Windows acts on exactly that: Ctrl+Left Shift sets
left-to-right and Ctrl+Right Shift right-to-left in RTL-aware editors, and Ctrl+Shift or
Alt+Shift switches the keyboard layout where that hotkey is enabled. byovox therefore taps one
unassigned virtual key (`0xFF`) the instant a chord fires: applications ignore a key nothing is
bound to, while the shell sees a key pressed with those modifiers and leaves the text direction
and the layout alone.

The hook sees every key on the desktop, including synthesised ones, so byovox stamps the
keystrokes it sends itself with a marker and ignores those: the Ctrl+V the `paste` rung sends
cannot read as a hotkey press, and neither can that tap.

No elevation, no driver, no service — the hook lives in the daemon's message loop and dies
with it.

## Insertion, and elevated windows

Three rungs, tried in the order below while `inject.mode = "auto"`:

| Rung | How | Notes |
|---|---|---|
| `type` | `SendInput` with `KEYEVENTF_UNICODE` | script-agnostic — Hebrew, Cyrillic and emoji land without a matching layout |
| `paste` | clipboard, Ctrl+V, previous clipboard **text** restored | a clipboard holding an image is replaced and not restored |
| `clipboard-only` | clipboard, nothing else | you press Ctrl+V |

UIPI — the Windows rule that a process cannot reach across integrity levels — meets byovox
before insertion does. A low-level keyboard hook installed by a non-elevated process is not
shown the keystrokes going to an elevated window, so while Task Manager's search box, an admin
PowerShell or an installer has focus the hotkey does nothing at all: no recording starts, the
tray icon stays grey, and the log gets no line. That is UIPI working, not a fault.

Dictating into elevated windows therefore means running byovox itself elevated, and that is a
bad trade to take lightly: it puts a global keyboard hook and a live microphone inside an
administrator process. byovox does not offer it.

A dictation can still start in an elevated window without the hook: `byovox toggle` starts one
with no key press involved. Then the same rule applies to insertion instead — `type` and
`paste` log `inject rung failed`, `clipboard-only` succeeds, and the done cue means *the text
is on the clipboard, press Ctrl+V*.

The text goes to the window that was in front when the hotkey was pressed. If focus moved while
you spoke or while the audio was transcribed, byovox brings that window back (restoring it if
minimised) and types there; focus stays on it afterwards. The window puts focus back where it
was itself; when that is not a text field (WhatsApp's comes back that way), byovox puts focus
back on the field the hotkey was pressed in. If it was closed (an app closed to the tray
counts, and so does a window left on another virtual desktop: byovox never switches desktops;
that case is untested), or Windows will not bring it back, nothing is typed anywhere, not even
onto the clipboard: the error cue plays and `byovox last` has the text. The window is the unit:
a click into another field of the *same* window moves the text with it.

A press made with nothing focused (the desktop or the taskbar) has no window to go back to,
so the text goes wherever focus is when it is ready, if that is something that takes text.
UI Automation decides this: an editable text box, Windows Terminal, a console and a browser
text field or rich editor count; a browser page body, a file list or a button does not.
Where UI Automation sees no text, a blinking text cursor (the system caret) in the focused
window counts instead: that is how Notepad++ and other Scintilla editors are recognised.
When focus is not on a text field, nothing is typed, the error cue plays and `byovox last`
has the text. An app that exposes no UI Automation and shows no caret reads as not a text
field.

No rung ever presses Enter: a newline in the text becomes a space before it is typed or
pasted, so a dictation into a chat box that sends on Enter stays one unsent message, and one
into a terminal stays one unexecuted line. That sanitising pass is also what keeps a spoken
enumeration on one line: the cleanup prompt often numbers it across several lines, and the
newlines become spaces before anything is typed.

## Language routing

The layout byovox reads is the **foreground window's**, taken from that window's own thread —
which is exactly what Windows' "let me use a different input method for each app window"
setting controls. Win+Space therefore changes the layout byovox reads — a hint under auto,
the forced language otherwise — only for the window you are in, not for the desktop.

Reading the layout is not the same as routing on it. Under `language.default = "auto"` the
layout is only a hint, for the dictations detection leaves open; when it is used is set out
once, in the `[stt.by_language]` comment of [`config.example.toml`](config.example.toml). The
shipped default leaves `[language.by_layout]` empty: without `he = "he"` in it, an unsure
dictation in a Hebrew window keeps the detected reply exactly like one in an English window,
and the log line says `lang=auto` either way. Map the layouts you dictate in and the hint
follows the window with nothing configured per app.

Force a language you are not speaking and whisper does not transcribe it, it **translates**:
Hebrew speech under `language.candidates = ["en", "ru"]` and no `he` lane (lane codes join the
list), on a server with the `language_candidates` extension, comes back as English, and
Russian speech sent with `language=he` comes back in Hebrew letters. That is the model doing
what it was asked, deterministically. Detection keeps a dictation it is sure of away from the
layout; one it is unsure of still takes the layout's language, which is the reason to keep the
layout matched to the language you are speaking rather than to the language you are writing
about.

## Microphone level

Windows "audio enhancements" (Settings → System → Sound → the input device → Audio
enhancements) can attenuate a microphone by as much as 30 dB, and laptop array mics often ship
with them on. The symptom is rarely plain silence: transcripts come back empty, or as one
hallucinated stock phrase. A capture with no 10 ms window above -40 dBFS RMS is not sent at
all: nothing is typed, no cue plays, and the log has one `silent capture discarded` line.

`byovox check` records a second and prints the peak and the loudest 10 ms window's RMS, e.g.
`peak -27.3 dBFS, loudest 10 ms -29.10 dBFS RMS over 0.8s`. The row keeps its `ok` mark and adds
an inline `← …` note when that window is at or under -40 dBFS — the same test the pipeline
applies. If a normal speaking voice reads anywhere near -40, turn the enhancements off for that
device and run `check` again.

`byovox setup` offers a fuller check at the end, on every platform: `Check the microphone level
now? [Y/n]`, default yes. It records 3 s of room tone, then 5 s of a spoken sentence, and prints
the room floor beside the sentence's 95th-percentile, loudest-window and peak levels. The floor
is only ever printed, never warned about — there is not yet enough data across rooms and
microphones to set a noisy-room threshold. The sentence gets "this quiet, byovox would discard
the dictation and type nothing" when it is as quiet as `Audio::is_silent`, or else "this level
is low" when its 95th-percentile window is within 10 dB of the floor or under -35 dBFS — never
both. Independently of either, it gets "clipping" when its peak is at or above -1 dBFS. The
first two both advise turning the input gain up, and, on Windows, also turning Audio
enhancements off for that device; clipping advises turning the gain down.

### Bluetooth headsets

Connect a Bluetooth headset and Windows makes it the default *recording* device. A headset
carries a microphone only in its hands-free profile, so opening a capture stream on it drags
the whole device out of A2DP (stereo, full bandwidth) into that profile — mono at 8–16 kHz —
for as long as the stream is open. With byovox that is every dictation: music in the
headphones jumps in pitch and volume on the press and jumps back on the release, and the
transcript is worse than the built-in array would have given.

The playback byovox does — the start, done, warning and error cues — is not the cause. An output stream
leaves A2DP alone; only a *capture* stream forces the switch.

Pin the microphone instead of unpairing the headset:

```toml
[capture]
device = "Microphone Array"
```

Any case-insensitive substring of the device's name will do, and a name given in full wins
over one that merely contains it: `Microphone` picks a device called exactly that, not
`Microphone Array`. `byovox check` prints the
names to choose from on its `inputs` row — they come from the audio host, so they are shorter
than the Sound control panel's (`Microphone Array`, `Headset`). A name that matches no device
is refused at startup with that same list, and `check` warns when the microphone it used is a
hands-free endpoint. A re-run of setup keeps `capture.device` too.

## Cue output

The start, done, warning and error cues play on whatever Windows has as the default output device, and
they follow it: byovox registers for endpoint notifications (`IMMNotificationClient`), so
switching the default in the Sound panel — or switching a Bluetooth headset off, which
switches it for you — re-binds the cues to the device that took over, with one INFO
`cue output re-opened after an audio device change` line per re-bind. A disconnect fires a
burst of notifications and the default only settles after the burst, so the re-bind is taken
half a second after the first of them — late enough that the burst is over, and capped so that an endpoint flapping without
pause is still re-bound once per half-second rather than postponed for ever.

Four things re-bind the cues: the default moving, an endpoint being removed, an endpoint
changing state, and a change to the default output's **shared-mode format** (device Properties
→ Advanced → Default Format), which invalidates every open stream on that endpoint without
moving the default or taking it out of service. Anything else that can kill a stream — another
application seizing the endpoint in exclusive mode, say — reports nothing to a listener, so it
is not covered.

The re-bind lands between cues, never during one: the tone after the switch is the first to
play on the new device. Without it the cues would simply stop — a stream whose endpoint has
gone keeps accepting tones without complaining — for the rest of that daemon's life. If no
output device is reachable at all, cues go quiet with a single WARN and every other layer
carries on; the next cue retries.

The tray's **Audio cues** item silences them for the running daemon and closes the output
stream with them. `indicator.cue` in the config is the value the daemon starts at, and the
item does not write back to it.

How loud they are is `indicator.cue_volume` (0.0 to 1.0, default 0.05): a fraction of full
scale, on top of the system volume. byovox's own slider in the Volume Mixer lowers them
further, for that machine only.

## Setup, start to finish

Four steps. Only the first two are required — byovox needs a speech server and nothing else.

### 1. Whisper server

**Let setup do it.** When nothing is listening on `127.0.0.1:8770`, `byovox setup`
([step 2](#2-byovox)) offers to do this step for you; the default is no. It downloads a
whisper.cpp `b5130` build — CPU (8.6 MB, the default), CPU with OpenBLAS (21.4 MB), or
NVIDIA CUDA 12.4 (675 MB, listed only when the NVIDIA driver's `nvcuda.dll` is in System32) —
then a model from that build's own menu: CPU and BLAS offer `ggml-small-q8_0.bin` (264 MB, the
default) then `ggml-base-q8_0.bin` (81.8 MB, fastest and roughest); CUDA offers
`ggml-large-v3-turbo` (1.62 GB, the default) then `ggml-large-v3-turbo-q5_0` (574 MB) — turbo
is too slow on a CPU to offer there, and BLAS is unmeasured. Each file is checked against the
size and SHA-256
pinned in byovox, and the downloaded server is started with `-t <threads>` for this machine's
logical core count. When Hebrew is among the languages you name, setup notes that the CPU and
BLAS models transcribe it poorly and points at a GPU server or a `[stt.by_language]` lane.
Setup runs `whisper-server -h` to prove the server starts before it downloads the model,
then probes the server with a second of silence, writes
[`[stt.server]`](#starting-everything-at-login) so the daemon runs it, and keeps the server up
for its closing `check`. If `-h`
fails because a DLL is missing, install the
[Microsoft Visual C++ 2015-2022 Redistributable (x64)](https://aka.ms/vs/17/release/vc_redist.x64.exe)
and run `byovox setup` again. The files land in `%LOCALAPPDATA%\byovox\data\whisper-b5130`
([removal](#where-things-live)). Run again, setup downloads neither a build it already unpacked
nor a model that is still there and still matches its pin, and it offers to keep any valid
server `[stt.server]` names (default yes) in place of the download — a hand-set one with its
`[stt]` URL, model and token.

If the server setup started does not transcribe that second of silence within `stt.timeout_s`
(30 s by default), setup says the model is likely too slow on this machine: pick a smaller
model, or the CUDA build on a machine with an NVIDIA GPU. To wait longer instead, answer `k`
to keep the server, then raise `timeout_s` under `[stt]` in the file setup writes; every
dictation can then wait that long. A re-run of setup keeps it.

The rest of this section is the same server by hand.

byovox posts to `{stt.base_url}/audio/transcriptions`, the OpenAI shape. whisper.cpp serves it
but **not at that path by default**, which is the single most common way this fails.

1. Download `whisper-bin-x64.zip` from
   [whisper.cpp releases](https://github.com/ggml-org/whisper.cpp/releases) and unzip it, e.g.
   to `C:\tools\whisper`. (This is **whisper.cpp**, not the unrelated "WhisperFlow" product.)
2. Download a model from
   [huggingface.co/ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp/tree/main)
   and drop the `.bin` beside the executables:

   | model | size | notes |
   |---|---|---|
   | `ggml-base.bin` | ~150 MB | fast, rough — fine for proving the pipeline works |
   | `ggml-small-q8_0.bin` | ~265 MB | the one for a machine without an NVIDIA GPU: about 7–8 s per auto-language dictation at whisper-server's default 4 threads on a 6-core desktop CPU, 4.5–5 s at 12 |
   | `ggml-large-v3-turbo.bin` | ~1.6 GB | closest to the large-v3 a GPU server runs, but too slow without a GPU: its q5_0 variant took 54–59 s per auto-language dictation on that CPU at 4 threads (39–48 s at 6), over the 30 s `stt.timeout_s`; the full model was not measured |

   Those figures, and the other models', are in [benchmarks](benchmarks.md).

3. Run it:

   ```sh
   whisper-server -m ggml-small-q8_0.bin --host 127.0.0.1 --port 8770 -l auto ^
     --inference-path /v1/audio/transcriptions
   ```

`--inference-path` is what puts it on `/v1/audio/transcriptions`. Without it whisper.cpp
listens on `/inference` and byovox reports `stt HTTP 404`. `-l auto` stops the server forcing
English and silently translating everything into it.

Add `-t 8` on a machine with cores to spare; the default is 4. `byovox setup` writes `-t`
equal to this machine's logical core count when it starts the server itself. Check the startup
banner for `whisper_backend_init_gpu: no GPU found` — CPU-only transcription of a long sentence
takes seconds, not milliseconds.

`byovox` then wants `stt.base_url = "http://127.0.0.1:8770/v1"`.

### 2. byovox

```sh
byovox setup
```

It ends by running `byovox check` on the file it wrote.

### 3. Text clean-up (optional)

Any `/v1/chat/completions` server. [Ollama](https://ollama.com/download) is the easy one:

```sh
ollama pull gemma4:e2b
```

**Ollama cannot do step 1.** Whisper models in its registry do not load — Ollama runs
llama.cpp, which has no Whisper support, and `/v1/audio/transcriptions` answers `HTTP 500`
with `unknown model architecture`. Clean-up only.

**Watch the model size.** This model is resident and runs on every dictation, so it competes
with whisper for the same CPU and RAM. On a laptop that struggles with a 4B model, a 7B or
larger one will make dictation slower than typing. Start small: `gemma4:e2b`, `llama3.2:3b`,
`qwen2.5:3b`. If clean-up takes longer than a few seconds, the model is too big for the
machine — polish failing or timing out is not fatal (byovox types the raw transcript instead),
but you paid the wait for nothing.

If nothing local is fast enough, Ollama's hosted models (`gemma4:31b-cloud` and friends) are
far better and cost nothing to try — **but your transcripts leave the machine.** `byovox setup`
lists them after the local ones, marked *hosted by ollama.com*, and writes one only after a
question whose default is no. Or say so deliberately in the config:

```toml
[polish]
base_url = "http://127.0.0.1:11434/v1"
model    = "gemma4:31b-cloud"          # proxied by Ollama to ollama.com
hosted   = true                        # the URL is loopback; the text still leaves the machine
```

`hosted` is what `byovox check` reads to print its `note hosted` row. The wizard sets it for
you on this pick; set it by hand here or the check stays silent about a stage that is not
local.

Only the transcript is sent, never audio, and only when polish is enabled. If that is not a
trade you want, keep a small local model or set `polish.enabled = false`.

### 4. ffmpeg (optional, and usually unnecessary)

**byovox does not need ffmpeg.** It sends 16 kHz mono WAV, which whisper.cpp reads directly.
You need it only if you pass `--convert` to `whisper-server` to feed it other audio formats
yourself — and if you pass `--convert` without it, the server exits at startup with
`ffmpeg is not found`.

If you do want it, unzip a Windows build from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/)
and add its `bin` folder to your user PATH.

## Starting everything at login

Two separate things have to come up: byovox, and the speech server it talks to.

**byovox** has it built in, and `byovox setup` turns it on. The tray's **Run on startup** item
turns it off and on again; so does the CLI:

```sh
byovox autostart --enable
```

**whisper-server** on this machine can ride along: name it in `[stt.server]` and the daemon
starts it whenever the daemon starts, at logon included, with no console window and its output
discarded. The keys, and a whisper.cpp example, are in
[`config.example.toml`](config.example.toml).

The server runs inside a Windows job object that ends it with the daemon: a tray Quit,
`byovox quit`, a crash and ending `byovox-daemon.exe` in Task Manager all take it with them.
Nothing restarts it. A server that exits leaves one WARN in the log with its exit code —
`0xc0000135` means a DLL it needs is missing, usually the Microsoft Visual C++ Redistributable —
and dictation then fails at `stt`. `byovox check` prints a `server` row with the command, and
fails it when the path is not absolute, not there, or not an `.exe` — and the daemon then
refuses to start.

The model takes a few seconds to load, so the server may not be ready for a dictation in the
first moments after logon. byovox does not care: it starts, and the first press once the server
is up works normally. `byovox check` is how you confirm both halves are running.

To watch it start, run the `whisper-server` command in a terminal by hand instead — the banner
ends with `whisper server listening at http://127.0.0.1:8770`.

## Autostart

`byovox autostart --enable` writes the quoted path of `byovox-daemon.exe` — the binary beside
the CLI that the tray actually runs in — followed by `--config "<absolute path>"` when you gave
one, to
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` under the value name `byovox`;
`--disable` deletes it. Per-user, no elevation, no installer. Move or rename the binaries and
the value has to be written again.

`byovox setup` writes the same value once it has written the config, carrying its `--config` the
same way; if that fails, setup prints why and still exits on its `check`. The tray's **Run on
startup** check shows whether the value is there and adds or deletes it, carrying the config the
running daemon was started on when that is not the default.

`byovox-daemon.exe` is a windows-subsystem binary: it has no console, so nothing flashes at
logon and closing the terminal that ran `byovox` cannot take the tray with it. `byovox run` is
the same daemon inside the console CLI, for watching one start.

## Where things live

| | |
|---|---|
| config | `%APPDATA%\byovox\config\config.toml` |
| logs (daily, seven kept) | `%LOCALAPPDATA%\byovox\data\logs` |
| capture log, when enabled | `%APPDATA%\byovox\data\capture` |
| update check stamp | `%APPDATA%\byovox\data\update.json` |
| whisper.cpp download, when accepted in `byovox setup` | `%LOCALAPPDATA%\byovox\data\whisper-b5130`: the model at the top, the build in `cpu`, `blas` or `cuda-12.4` |

To remove the whisper.cpp download, quit byovox, delete `%LOCALAPPDATA%\byovox\data\whisper-b5130`,
and set `command = ""` and `args = []` under `[stt.server]` in the config, with `stt.base_url`
pointed at another server.

Transcript text is logged only at `logging.level = "debug"`; at `info` the log carries
timings, the language sent and the rung used, never what you said. The capture log is the
exception: with `capture_log.enabled = true` it stores every raw and polished transcript, by
design — that is what it is for.
