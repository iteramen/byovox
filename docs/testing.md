# Manual checklist

Run this against the release build (`cargo build --release`) before a tag is pushed — that
binary is what the release workflow publishes, and nothing else here is exercised by CI.
Run `byovox check` first; every line below assumes it passed. The boxes stay unticked — this
is the template for each release run, not a record of one.

## Hotkey
- [ ] hold → release: text inserted; the pill showed recording then working
- [ ] tap (< 250 ms): nothing happens, no request in the log
- [ ] hold, then Escape: recording discarded, no request
- [ ] hold 2 s with the microphone muted (Windows privacy toggle, or the device's own mute): nothing typed, one `silent capture discarded` line in the log, no request. This is the only check that a muted microphone really delivers zeros
- [ ] hold 3 s: exactly one `Pressed` (no auto-repeat), one dictation
- [ ] `mode = "toggle"`: press starts, press again sends
- [ ] `byovox toggle` twice from a terminal: one dictation

With `key = "ControlLeft+ShiftLeft+Z"`, into Notepad:

- [ ] hold the chord and speak: one dictation, and **nothing** typed into the window — no `z`,
      and a Hebrew paragraph keeps its direction (a bare Ctrl+Left Shift flips it)
- [ ] plain `Z`, no modifiers: types a `z`, no recording starts
- [ ] hold the chord, then let a modifier go first: recording ends and the text is inserted,
      and no `z` leaks when the trigger finally comes up

## Language routing

With `language.default = "auto"`, `candidates = ["en", "ru"]`, a `[stt.by_language.he]` lane,
`[language.by_layout] he = "he"`, `stt.no_speech_threshold` above 0 (detection reads the
`verbose_json` reply the gate asks for) and `capture_log.enabled = true` (see
[`docs/config.example.toml`](config.example.toml)). The shipped default leaves the lane and the
map out, so every row below would log `auto`.

- [ ] `byovox check` on the Hebrew layout: the `layout` row reads `he → language_candidates=en,ru,he (prior he)`; on the English layout it has no `(prior …)`
- [ ] Notepad with an English layout, speak English: log line says `lang=auto`, English comes back
- [ ] same window, still on the English layout, speak a Hebrew sentence: log line says `lang=he` and Hebrew comes back — detection routed it; its capture row has `detected: "he"` and `detected_p` of 0.5 or more
- [ ] switch that window to Hebrew (Win+Space) and speak a Russian sentence: log line says `lang=auto` and Russian comes back in Cyrillic, not in Hebrew letters
- [ ] on the Hebrew layout, dictate short Hebrew clips until a capture row has `detected_p` under 0.5 (or `detected: null`): that dictation logs `lang=he` — the prior decided — and Hebrew comes back
- [ ] a *different* window on Hebrew while Notepad stays English: an unsure dictation into Notepad still logs `auto` (per-window layout)
- [ ] `stt.base_url` pointed at a port nothing listens on, on the Hebrew layout, speak Hebrew: the log has a WARN `stt failed; sending the layout's language to its lane` with `lang=he`, and the `he` lane serves the dictation (`lang=he`, Hebrew comes back)
- [ ] `stt.no_speech_threshold = 0` (and `no_speech_warn = 0`), on the Hebrew layout: the dictation logs `lang=he` and reaches the `he` lane in one request

## Insertion
- [ ] Notepad, a browser text field, Windows Terminal, VS Code: text lands with all scripts intact
- [ ] an elevated window (Task Manager's search, or an admin PowerShell): the hotkey is ignored while it has focus — no recording starts, the tray icon stays grey, no line in the log
- [ ] `inject.mode = "paste"`: clipboard content from before the dictation is restored afterwards
- [ ] dictate into Notepad, and while it transcribes click into another window: focus jumps back to Notepad and the text lands there; repeat with Notepad minimised: it is restored and gets the text; repeat in WhatsApp Desktop's message box, minimising it: it is restored and the text lands in the message box; repeat closing Notepad, or closing WhatsApp to the tray, instead: nothing is typed anywhere, error cue, `byovox last` has the text, and the log has a WARN `no window to type the dictation into`
- [ ] click the desktop, press the hotkey, then click into Notepad while speaking: the text lands in Notepad; again, but click a browser page body (not a field), or a File Explorer file list: nothing is typed, error cue, `byovox last` has the text; again into Windows Terminal: the text lands
- [ ] click an empty spot on the taskbar, press the hotkey, then click into Notepad while speaking: the text lands in Notepad; repeat on a secondary monitor's taskbar
- [ ] polish endpoint down (wrong port): raw transcript inserted, error cue, WARN in log
- [ ] `inject.mode = "type"`, focus a chat box that sends on Enter, dictate "first foo, second bar": one *unsent* message reading `1. foo, 2. bar` — the comma is rule 3's own printed example echoed back, not the model's choice: remove it from the example and the output loses it — nothing is submitted, no stray leading or trailing space
- [ ] dictate a plain sentence: the text lands with no final period; dictate a question: the `?` stays; with `inject.strip_terminal_period = false` the period is typed
- [ ] dictate "rename it to tokenizer period strip" and read the raw transcript in the log first: **when whisper wrote the words undotted**, `tokenizer.strip` is typed, not the three words; dictate "the Jurassic period ended" → `period` stays a word. "training period strip" is the known limit, not a regression: "training period" is an English noun phrase, so the sentence gives no more evidence than the Jurassic one does and the words stay — as they also do when whisper dotted them itself (`training.period.strip`)
- [ ] with `polish.capitalize_first_word = false`: dictate "we should ship it today" → it lands lowercase; dictate "NASA confirmed it" and "I think we ship" → the capital stays, and stays when the model lowers it. With a `polish.prompt_file` set, the flag changes nothing — that file owns its own rule 1
- [ ] with `[stt.by_language.he]` configured: `byovox check` shows an `ok stt[he]` row; dictate "אני עוד רבע שעה אצלו" on any layout → `רבע שעה` (the default lane types `רבשה`); on the Hebrew layout mapped to `he`, dictate one word ("תודה") → it still lands, and the log shows `stt lane returned nothing; retrying the default endpoint`
- [ ] with a glossary in `stt.prompt`: dictate "שמנו את Tailscale מאחורי אותו proxy" under a Hebrew layout → `Tailscale` lands in Latin; dictate "ראיתי אריה בגן החיות" → `אריה` stays Hebrew

## Indicator
- [ ] first press after the daemon starts: the recording cue is audible (the output device is opened at start, not by that press)
- [ ] dictate a quiet mumble from across the room: the text still lands, the completion cue is the lower warning tone (not the short done blip), and the log has a `stt.no_speech_warn` WARN line with the probability
- [ ] tray icon: a mic glyph — grey idle, red recording, amber working with a dot orbiting it, magenta for ~3 s on error; Disable shows the red ✕ over the grey mic
- [ ] tray menu: Disable stops the hotkey; Toggle mode takes its check mark and the hotkey then starts/stops on separate presses; Copy last transcript, then Ctrl+V in any text box, pastes the last dictation (with none yet, the clipboard is left as it was); Open config / Open logs open Explorer; Run check opens a console that shows every row and waits for Enter; Quit exits
- [ ] with a chord hotkey and Disable active: pressing the chord types its trigger into the window (nothing is swallowed); Enable → the chord dictates again
- [ ] Run check under `byovox run`: the rows land in *that* terminal, interleaved with the live log, and its "press Enter" competes with the shell for stdin — expected; run the menu item against the background daemon
- [ ] tray menu, Audio cues: clear it and the next dictation is silent; check it again and the cue after that is audible (not the one straight after the click — the click is what opens the device)
- [ ] the pill never gets a taskbar button, on the first show and after a hide/re-show
- [ ] `indicator.pill = false` and `cue = false` each remove exactly that layer — and with `cue = false`, checking Audio cues from the tray brings them back for this daemon without editing the file; `cue_volume` sets the level of all four cues after a restart (`0.0` plays them silent)
- [ ] with cues audible on a Bluetooth headset, switch the headset off mid-session: one `cue output re-opened after an audio device change` line in the log, and the next dictation's cues play on the device that took over; switch it back on and they follow it back

## Operations
- [ ] second `byovox` → "already running"
- [ ] close the terminal that ran `byovox`: the tray survives; `byovox run` and close its terminal: the tray exits
- [ ] `byovox status` / `byovox last` reflect the last dictation
- [ ] `capture_log.enabled = true`: WAV + JSONL row appear per dictation; transcript text absent from the INFO log
- [ ] a dictation held past 60 s, one sentence at the start and silence after, to a whisper-server started with `--vad-model`: the sentence comes back once, not repeated. To one started without it: a WARN `stt refused vad=true for a long take; re-sending without it` in the log, and the text still comes back
- [ ] `byovox autostart --enable`, sign out/in: daemon running; `--disable` removes it
- [ ] `byovox setup` to the end prints `autostart enabled`, and the tray's Run on startup is checked; clear it → `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v byovox` finds nothing, and after sign out/in no daemon runs; check it again → the value is back
- [ ] `[stt.server]` pointing at a real whisper-server (the example in [`docs/config.example.toml`](config.example.toml)): `byovox` starts it with no console window and `check` passes with an `ok server` row; a tray Quit leaves no `whisper-server.exe` in Task Manager, and neither does ending `byovox-daemon.exe` there; a bad model path in `args` leaves one `stt server exited` WARN with an exit code in the log
- [ ] with that hand-set `[stt.server]` moved off 8770 (outside `%LOCALAPPDATA%\byovox\data\whisper-b5130`; `--port 9100` in `args` and `stt.base_url = "http://127.0.0.1:9100/v1"`) and the daemon running, `byovox setup` again → overwrite `y` → Enter at `Keep the speech server your config names (…) and its [stt] URL, model and token?`: no second `whisper-server.exe` appears in Task Manager, the probe and `check` pass with no `waiting for` line, and the file still names the same `command`, `args`, `base_url`, `model`, `api_key_env` and `api_key_file`. With the daemon stopped, the same run prints `waiting for http://127.0.0.1:9100/health while the server loads its model...` before each of the probe and `check`, and both pass without a `did not answer` line. Answering `n` there goes to the source question, no download is offered, and the file gets `command = ""`
- [ ] update check, with `%APPDATA%\byovox\data\update.json` deleted and the daemon restarted with the network off: the log has one INFO `update check failed` line and the tray is unchanged; restart again within the day → no second INFO line and no request. Against a published release newer than the build: the status line ends `· <version> available`, a menu item "Open the byovox <version> release page" appears under it and opens the page in the browser, and one balloon names the version; set the stamp's `checked` to 0 and restart → the status line and item again, no second balloon. With `update.check = false`: no line, no stamp file, and `check`'s `update` row says `off`
- [ ] unknown key in config: exit 2 naming the key, from the bare `byovox` as well as from `byovox run` — and no daemon left behind
- [ ] `byovox setup` on a fresh config: probes pass, file written with comments intact, `check` passes
- [ ] `byovox setup` naming `he` as a candidate or a layout target: the Hebrew note prints once — at the model menu on a re-run whose old file names `he`, otherwise before the lane questions when this run downloaded a CPU or BLAS build; then one notice saying what a lane receives — and that it gets no token unless it names its own `api_key_env` or sets `inherit_token = true` — then one lane question per language named; a URL for `he`, then Enter at its model question, writes `he = { base_url = "…" }` under `[stt.by_language]` (with `, inherit_token = true` when `[stt]` has an `api_key_env` and the URL has its scheme, host and port), and the closing `check` prints an `stt[he]` row. Naming no language prints `No per-language speech-to-text endpoint offered` and asks no lane question
- [ ] `byovox setup` on a Windows machine with nothing on port 8770 and `%LOCALAPPDATA%\byovox\data\whisper-b5130` deleted → `y` → CPU → small-q8_0 (Enter): both downloads verify, `-h` runs, the server starts with `-t <logical cores>`, and then either the probe passes and `byovox` dictates with no console window, or, on a CPU several times slower than [benchmarks](benchmarks.md)' 2026-09-28 round (small-q8_0 took 4.4 s for the probe and 4.5–4.9 s per dictation there, at `-t 12`, what setup now writes on that 12-thread machine), the probe fails with the too-slow note. Record the seconds per dictation for a 5 s utterance on that machine
- [ ] the same with CUDA on an NVIDIA machine, turbo (Enter): no `no GPU found` warning
- [ ] after that, with the daemon running, `byovox setup` again → overwrite `y` → Enter at `Keep the local server set up earlier (…)?`: the probe and `check` pass, and the file still names the server under `[stt.server]`. Then with the daemon stopped and the config deleted but the download folder kept: `y` → CPU → small-q8_0 (Enter) downloads nothing
- [ ] a pin pointed at a wrong hash in a scratch build (or one byte of a `.part` file changed mid-download): the mismatch error names both hashes, and no final file is left
- [ ] `byovox setup` picking *a hosted service* for a stage: the line naming what leaves the machine is shown before the URL question, the file gets `hosted = true` under that section, and `check` then prints a `note hosted <host>` row for it and none for the other stage; setting `hosted` by hand does the same, and an `https://` endpoint with `hosted = false` prints neither that row nor `warn network`
- [ ] `python bench/polish_bench.py` (`python3` on Linux and macOS): every stratum passes on every run. The run prints the endpoint's generation rate first and refuses below 20 tok/s (`--allow-slow-endpoint` to override): a model that has fallen back to CPU answers with *different text*, not merely slower text. A rule change is scored here (`--candidate <file>`, `--prompt-file <file>` for a copy), never eyeballed
- [ ] `python bench/polish_bench.py --no-capitalize`: scores `capitalization` and nothing else — its expected texts start lowercase, so they are never scored beside the four above. Every item passes on every run. A selection that lands on the empty half (`--only` crossed with the wrong one) exits 2 rather than passing with nothing measured
- [ ] with a Bluetooth headset connected as the default recording device and `capture.device = "Microphone Array"`: `check`'s `mic` row names the array at 48 kHz, and a dictation leaves music in the headphones untouched (no pitch jump on press or release); a name that matches nothing → exit 2 listing the devices
- [ ] tray menu, Microphone: it lists System default and every input by its full name, the one in use checked; plug a USB mic in or out and reopen the menu — the list follows. Pick one: config.toml's `capture.device` holds its full name, the next dictation records from it (`check`'s `mic` row names it), and the other keys and comment lines are unchanged (a comment at the end of the `device` line itself is dropped); pick System default → `device = ""`
- [ ] `byovox setup`'s closing microphone level step, Enter at the offer: stay quiet for the 3 s floor capture, then speak a sentence for the 5 s one — the floor prints with no warning, and a normal speaking volume prints neither "this quiet, byovox would discard the dictation" nor "this level is low" nor "clipping". Repeat muted or very quiet → "this quiet, byovox would discard the dictation"; repeat close to the mic at high volume → "clipping". `n` at the offer skips straight to the closing `check`

## Read-aloud qualification (2026-11-08)

`cargo run --locked --example read_aloud_probe -- <own-TTS-base>` exercised the real HTTP client
and decoded the live Kokoro PCM16 mono/24 kHz response. Its streaming RIFF/data length sentinels
first exposed a compatibility failure; both client decoders now have a regression fixture.

The manual Windows `read_aloud_desktop_probe` used only its own synthetic English paragraph,
production UIA selection/TTS/rodio paths and native registered Ctrl+Alt+R. It passed selected
range → live TTS → active playback; a second configured shortcut stopped it, with no playback
1.5 seconds later and zero dictation events. The owner reported Speaking. No clipboard, mic,
ordinary document, production config or production daemon was used.

Early hook-based fixture attempts did not trigger read actions and are excluded. Native hotkey
registration delivered the actions. The probe now displays Requesting speech before playback,
so a second press during synthesis is clearly a cancel. Earlier repeated presses cancelled pending
requests, rather than proving a synthesis failure. Computer-use automation could not initialize;
the owner performed selection and shortcut presses. No injected-input test is counted as a pass.

A separate `read_aloud_registration_probe` passed a real OS registration conflict, its actionable
failure event, Disable releasing the chord to another registration, and Enable reacquiring it.
This uses registration only, not injected input or a full tray-menu test.

Remaining manual checklist: empty/unsupported/password controls; changed foreground during
selection; Escape/Disable and dictation interruption; shortcut conflict/disabled state; long and
mixed-language speech; elevated/custom applications; actual output-device loss. Hebrew/Russian
voices/backends remain unconfigured and unqualified; unit tests cover routing and refusal without
English fallback. The full daemon/release UI is not implied by the isolated fixture pass.

## Contextual shortcut follow-up (2026-11-09)

Owner smoke test of the installed PR build exposed a chat selection miss: focused-composer-only
lookup reported Select text first despite highlighted message text. The owner ruled that the
default shortcut should select read/dictate behavior from selection state. Lookup now also checks
foreground document providers without reading unselected content. Unit cases cover an empty
focused composer plus a selected document, duplicate/conflicting ranges, limits and consuming
a reading press's release before a later ordinary dictation hold. Same-chord and bare-modifier
configuration are covered; the optional separate binding remains tested separately.

The reported dictation attempts reached capture but were discarded as silence, including one
prior to the PR installation. Device metadata identified the configured generic Microphone as
Microphone (tranScreen Audio). The owner selected Microphone Array (Synaptics Audio) instead.
This is an input selection correction, not a reason to bypass the silence gate. Device enumeration
uses `audio_input_probe` without opening a recording stream. Actual shared-shortcut chat read,
empty-selection microphone dictation and output insertion need an owner-operated smoke check of
the newly installed build; unit/CI success alone does not close that acceptance.

The first contextual owner retry still refused both actions on a provider lookup error. That
version remains a failed smoke result, not an accepted fix. The follow-up replaces walking
unrelated UI controls with one filtered snapshot of document providers (at most 32 returned)
and retains bounded lookups and fixed/numeric-only errors. Dictation was temporarily restored
without the contextual lookup while this repair was prepared.

The owner also reported the failed pill below the taskbar. Placement now uses the cursor
monitor's physical work area (excluding taskbars/app bars), flips above/left as needed, clamps
inside that area, and repositions after DPI/size changes. Geometry tests cover all four taskbar
sides, edges, negative monitor origins and a scaled pill. Actual placement and contextual
read/dictate behavior still need the owner-operated check on the installed follow-up.

The next owner retry confirmed dictation in the chat client after the physical-input correction,
but reading still failed and WhatsApp contextual dictation was blocked. The numeric error was
HRESULT S_OK (0): UIA's normal successful null-pattern result is represented by windows-rs as
Error::empty(). Treating it as a failed provider was incorrect. A regression test first failed
for S_OK, then passed after normalizing successful-null/unsupported results to provider absence;
access-denied and unavailable-element errors still fail closed. The owner reported the updated
pill visible near the cursor, offset right/above. Reading and WhatsApp require a fresh native check
of the null-pattern fix; these failed owner attempts are not accepted as passing evidence.

The subsequent owner retry still failed reading and WhatsApp dictation; status identified the
document-count guard, which had been applied before off-screen providers were excluded. The
server-side UIA condition now combines Document control type with IsOffscreen=false before
applying the 32-provider limit. The Windows contract also permits GetSelection to return a
successful null array when there is no selected range/insertion point; that is normalized to
no selection, not a provider error. Its regression test was run red then green. See the
[Microsoft GetSelection contract](https://learn.microsoft.com/en-us/windows/win32/api/uiautomationclient/nf-uiautomationclient-iuiautomationtextpattern-getselection).
Native reading/WhatsApp acceptance remains pending; ordinary dictation was restored separately
while these repairs were built.

The active bound is 128 returned visible document providers (with the same time budget), rather
than limiting total documents in a long conversation to 32 before checking visibility. No text is
read while filtering providers; only explicit selected ranges are fetched afterwards.

## Separate-function qualification (2026-11-10)

The owner withdrew the shared shortcut for now and requested independent tests. The local
configuration uses CtrlLeft+ShiftLeft+Z for dictation and CtrlLeft+AltLeft+R for narration; the
binary and physical microphone were unchanged during that separation. WhatsApp dictation then
passed (text landed). Preserve this known-working path while debugging narration.

Narration in Notepad failed with no selection state exposed. The fallback query had filtered
Document control roles, which can miss selection providers exposed as Edit or other control roles. It
now filters IsTextPatternAvailable=true and IsOffscreen=false instead. GetText remains restricted
to explicit non-degenerate selected ranges, with the same bounds/ambiguity/password/origin checks.
Notepad narration acceptance of this follow-up is pending; the previous failed attempts are not passes.

## Separate dispatch repair (2026-11-10)

Issue #37. Default-binding regression failed before the change. The product now uses a
separate default binding, resolves old empty bindings to Ctrl+Alt+R, and rejects overlap.
Unit checks cover recording/queued-action/disabled admission without selection access,
cancellation during blocked lookup without holding the pipeline mutex, and post-lookup
admission. Selection runs on its own bounded worker, not on the action dispatcher.
The no-selection error now includes only numeric role/provider-count diagnostics.

Notepad native narration remains failed at the previous build. Re-test after installing
this repair and use its diagnostics to investigate; green CI is not native acceptance.
