# Security policy

byovox installs a global keyboard hook, opens your microphone, sends audio to a server you
configure, and types that server's answer into whatever window has focus. That is a large
amount of trust to hand a small program. This file says exactly where the boundaries are.
What byovox sends and stores, stage by stage: [docs/privacy.md](docs/privacy.md).

## Reporting a vulnerability

Report privately through GitHub's **private vulnerability reporting**: the *Security* tab of
this repository → *Report a vulnerability*. It is enabled under
*Settings → Code security → Private vulnerability reporting*; if the tab shows no such
option, open an issue asking for a contact address rather than posting details publicly.

Please include the byovox version (`byovox --version`), your OS, and the smallest steps that
reproduce it. There is no bounty. Expect a first response within a week.

Do not report the items under [Accepted by design](#accepted-by-design) — they are known and
documented. A concrete way to *break* one of those boundaries is a vulnerability and is very
welcome.

## Supported versions

`main` only, until a 1.0 release exists. Fixes land on `main`; there are no backports and no
maintained release branches. Build from source or use the most recent tag.

## Threat model

The user running byovox is trusted. Everything below is written from the point of view of an
attacker who is *not* that user, except where a row says otherwise.

| Boundary | What byovox does | In scope | Out of scope |
|---|---|---|---|
| **Keyboard hook** | `WH_KEYBOARD_LL` sees every keystroke in the session. Each event's virtual key is compared against the configured hotkey, chord members and cancel key, and its `dwExtraInfo` against byovox's own injection marker (see **Injection marker** below); nothing else is read from it. No key is logged, stored, buffered or transmitted at any level — there is no `trace!` call in the tree, and the hook module logs only hook-installation failures. Only a chord's trigger is swallowed; every other key reaches the focused window. | A key event reaching a log, a file, the network, or `byovox last`. Swallowing a key the hotkey does not claim. | That the process *can* see keystrokes: that is what a global hotkey is. Anything with the same user account can install its own hook. |
| **STT / polish endpoint** | The transcript the server returns is **typed into the focused window** as synthetic keystrokes, or pasted. One sanitising pass runs first, on the text about to be injected, so every rung is covered: it turns every `\n`, U+2028 and U+2029 into a space, removes all other control characters (C0, DEL and C1 — tabs, carriage returns, terminal escapes) and every Unicode format character (category `Cf`: the bidi overrides and isolates, zero-width spaces, the byte-order mark, the tag characters), and logs a WARN with the *count* dropped, never the text. Bidi marks and joiners (RLM/LRM, ZWJ/ZWNJ) are kept — Hebrew, Arabic and emoji are made of them. A cosmetic strip of exactly one terminal `.` runs at the same point, unless `inject.strip_terminal_period` is off (it defaults on). A reply left empty by either ends as an empty dictation. No rung ever presses Enter: a newline the server sends cannot submit a chat message or a shell line. A transcript longer than `inject.max_chars` (default 20 000) is **not typed at all** — it is held whole for `byovox last`, never truncated. | A byovox bug that types something the server did not send; a control or format character, other than the kept marks and joiners, reaching the keyboard; the endpoint reaching beyond the focused window (running a command itself, writing files). | That a malicious endpoint can type arbitrary **text** into the focused window. Sanitising removes the keystrokes nobody speaks; it cannot tell a harmful sentence from a harmless one. **You are trusting that server as much as you trust your own keyboard.** Point byovox only at servers you run or trust — every `stt.by_language` lane is one more of them, and receives a bearer token only when it names its own `api_key_env` or sets `inherit_token = true`, which sends `[stt]`'s. In auto mode with the no-speech gate on (`stt.no_speech_threshold` above 0, the default), every dictation reaches `[stt]` first, including the ones a lane then serves; with the gate off, a dictation on a layout mapped in `language.by_layout` goes straight to that language's lane when it has one. A hosted service for either stage is a supported choice, named as such by the wizard before you answer and recorded in `stt.hosted` / `polish.hosted`; a hosted speech endpoint receives your audio and a hosted polish endpoint every dictation's text. |
| **Secrets** | The bearer token comes from an environment variable, or a `NAME=VALUE` line in a key file. It is never logged, never put in an error string, and never printed by `byovox config` — the config file holds only the variable's *name* and the file's *path*. That holds when a server, or a proxy in front of it, echoes the `Authorization` value back as sent: an error message built from a reply body or from a transport cause has every occurrence of the token the request sent replaced with `[redacted]`, before a reply body is cut to 200 characters. The HTTP client structs derive no `Debug`, so the token cannot reach a log through `{:?}`. | The token appearing in a log file, console output, a crash message, or a capture-log row. | The key file's permissions, which are whatever you set — byovox neither checks nor changes them, and the example config tells you to keep the file readable by you alone. Another process running as you can read your environment. A server that re-encodes the token before echoing it (a JSON encoder writing `/` as `\/`, say) is not recognised, and that echo reaches the error message. |
| **Local IPC** | A per-user named pipe (`byovox-<user>.sock`) serving `status`, `last`, `toggle`, `quit`. It is created with the default named-pipe security descriptor, which grants full access to SYSTEM, Administrators and the creating user, and **read only** to Everyone and Anonymous. Every command must write a request line before the daemon answers, and writing requires an access right Everyone does not have — so a lower-integrity process (a sandboxed browser renderer) can neither open the microphone with `toggle` nor read a transcript with `last`. Remote clients are rejected outright. | A non-owner process sending any command, or reading a transcript. Reaching the socket over the network. | Processes running as the same user — they own the session, can read byovox's memory and can install their own hook. Denial of service: see [Known limitations](#known-limitations-accepted). |
| **Capture log** | Off by default. When `capture_log.enabled = true`, every dictation writes a **WAV of your voice** and a JSONL row containing the **raw and polished transcript in plain text** (the raw text is what the endpoint sent, with whisper's segment line breaks joined on a space, before sanitising) to `%APPDATA%\byovox\data\capture`. Files inherit the directory's permissions. Captures older than `capture_log.keep_days` (default 30) are deleted at startup and after each dictation; `0` keeps them for ever. Pruning is driven by the JSONL index and never by scanning the directory: a WAV is deleted only because an expired row names it, and only if that name also matches the exact form byovox writes. A file no row claims is never touched, whatever it is called — with one exception the pruner names as such: a `dictations.jsonl.tmp-*` file left behind by an interrupted prune is swept at startup, matched by that prefix rather than by any row. | byovox writing capture files when the setting is off, somewhere other than the configured directory, or keeping them past `keep_days`. Pruning deleting or corrupting anything byovox did not create. | The files themselves being readable by you and by anything running as you. Unbounded growth if you set `keep_days = 0` — that is the setting's meaning. |
| **Log file** | `%LOCALAPPDATA%\byovox\data\logs`, daily rotation, 7 files kept. At the default `info` level no transcript is written. At `debug`, the raw and polished transcripts are written to the log, along with up to 200 characters of any error response body the server returned, with the request's bearer token replaced by `[redacted]`. | A transcript reaching the log at `info` or above, or reaching the console or the tray. | What `debug` records: it is a diagnostic level you opt into. |
| **Clipboard** | The `paste` rung puts the transcript on the clipboard, sends Ctrl+V, and restores the previous **text** ~150 ms later. A clipboard holding an image or files is replaced and not restored. `clipboard-only` leaves the transcript on the clipboard indefinitely, and so does the tray's *Copy last transcript*, which puts it there only when you click it. | byovox failing to restore text it saved. | Clipboard managers and Windows clipboard history (Win+V) capturing the transcript, and **Windows Cloud Clipboard syncing it to your Microsoft account and other devices** if you have that on. Any process can read the clipboard during the paste window. |
| **Network** | Over plain HTTP your **audio, your transcript and your bearer token all cross the network in clear text**, and anyone on the path can replace the response with text byovox will type. `http://` to a non-loopback host is allowed and called out: `byovox check` prints a `warn network` row per affected endpoint, and the daemon logs one WARN at startup. Loopback is quiet. Separately from encryption, `check` prints a `note hosted <host>` row for each stage whose `hosted` key is set *and* which the daemon will actually call — a disabled polish stage gets no row, the same skip the cleartext rows use — naming the host and what that stage sends it. A `stt.by_language` lane cannot be marked hosted today; the key exists on `[stt]` and `[polish]` only. That row comes from the key `byovox setup` writes when you pick a hosted service, never from the URL: `https://` to your own tailnet is not hosted, and a hosted API behind a local gateway alias does not look it. It is a `note`, not a warning — a hosted endpoint is a choice byovox exists to allow. HTTPS is verified normally against a bundled Mozilla root store; there is no switch to disable verification. Beyond the endpoints you configure, and the whisper.cpp build and model that `byovox setup` downloads once, only when you say yes (see **Local server**), byovox makes **one anonymous version request to api.github.com per day**: the daemon, when it starts and at most once in 24 hours, sends an unauthenticated `GET` for the latest release over the same verified HTTPS, with a `byovox/<version>` User-Agent and no other header byovox sets. It carries no identifier, no config and no usage data, and nothing is downloaded or installed. `update.check = false` turns it off, and `byovox check` prints an `update` row saying which it is. | A missing or weakened certificate check on an `https://` endpoint; a cleartext endpoint that is *not* reported. The version request carrying anything beyond the version, being made with `update.check = false`, or being made more than once in 24 hours. | Using `http://` after being told — a reasonable choice to `localhost` or across a WireGuard/Tailscale link, a bad one across anything else. `check` still exits 0: this is a warning, not a failure. A privately-signed certificate will not verify, because the roots are the bundled Mozilla set, not the Windows store. GitHub seeing your IP address and byovox version once a day while the version request is on. |
| **Autostart** | `byovox autostart --enable` writes one value to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` holding an absolute path to `byovox-daemon`, plus any `--config` you gave, absolutised. | byovox writing outside HKCU, or registering a relative path that would resolve elsewhere at logon. | That the registered binary sits somewhere you can write. Anything able to overwrite it is already running as you, and could add its own Run value instead. |
| **Local server** | Off by default. When `stt.server.command` is set, the daemon starts that executable with `stt.server.args` — no shell, each list item one argument — as you, with no console window, inside a Windows job object that ends it when the daemon exits, crashes or is killed. Its stdin, stdout and stderr are all null: nothing it prints reaches byovox's log. It is never restarted; its exit is one WARN with the exit code. The daemon refuses to start on a `command` that is not an absolute path to an existing file, or on Windows not an `.exe` (a `.bat` or `.cmd` would run through `cmd.exe`). `byovox setup` can fill both keys in: when nothing answers on `127.0.0.1:8770` it offers, default no, to download a whisper.cpp `b5130` build and a model. Each file's URL, size and SHA-256 are pinned in byovox's source; it is fetched over HTTPS only, verified against the bundled roots, and written only under `%LOCALAPPDATA%\byovox\data\whisper-b5130\`. The arguments setup writes bind the server to 127.0.0.1 only. A re-run of `byovox setup` offers, default yes, to keep the server `command` names in place of the download offer: one it downloaded earlier, or any other the daemon would start (absolute, there, on Windows an `.exe`). On a yes it starts that `command` with its `args` for its probe and again for its closing `check`, ending it after each, unless something already answers on the host and port `stt.base_url` names. How a download is checked and unpacked, and how to remove it: [Local server download](#local-server-download). | byovox starting anything while `command` is empty; the server outliving the daemon; the server's output reaching byovox's log. A download kept without matching its pin, fetched over plain HTTP, or written outside that folder; anything downloaded without a yes. | What that executable does. It runs as you, and anything that can edit your config can already add its own Run value — the same boundary as the Autostart row. The whisper.cpp build and model are third-party artefacts that byovox neither builds nor attests: the pin proves a download is the file checked when it was pinned, not that the file is safe. |
| **Injection marker** | Events byovox synthesises carry a public constant in `dwExtraInfo`; the hook ignores events carrying it, so byovox does not react to its own typing. | byovox failing to stamp an event it sends, which would make it react to itself. | See [Known limitations](#known-limitations-accepted). |
| **Supply chain** | `Cargo.lock` is committed and CI builds `--locked`, including the audit tools' own installs. `cargo audit` and `cargo deny check` run on every pull request and every push to main; a vulnerability fails the build. A separate job type-checks (`cargo check --all-targets --locked`) against the declared MSRV, so `rust-version` is a checked promise rather than a claim. Every tag is built by a release workflow that publishes **a SHA-256 checksum beside the archive and a GitHub build-provenance attestation over the archive and over each binary inside it** — `sha256sum -c SHA256SUMS` and `gh attestation verify <zip> --owner iteramen` tie a download to that workflow and to the commit it ran on. Release binaries are **not code-signed**. | A dependency vulnerability that CI should have caught; a build that does not match `Cargo.lock`; a published artefact whose checksum or attestation does not match what the workflow built. | SmartScreen warning on first run — the expected result of an unsigned binary; `cargo install` builds on your own machine and avoids it. Authenticode signing needs a paid certificate and is not committed to. |

### Local server download

What `byovox setup` does after a yes to its whisper.cpp offer:

- Each file is streamed to a `.part` file that is renamed only when its size and SHA-256 both
  match the pin; a mismatch is deleted, never run. Reading stops one byte past the pinned
  size, and a download that receives nothing for 30 s fails.
- A file already there is checked against its pin again before it is kept, and a build
  already unpacked is not fetched again.
- From a build zip only `whisper-server.exe` and the `.dll` files in its `Release` folder are
  unpacked, flat.
- Setup runs the server once with `-h`, then for its probe and again for its closing `check`,
  ending it after each, unless something already answers on the host and port `stt.base_url`
  names.
- Removal is manual: delete `%LOCALAPPDATA%\byovox\data\whisper-b5130\` and clear
  `[stt.server]`.

### Accepted by design

These are properties of what byovox is, not bugs. They will not change:

- The configured endpoint can type arbitrary text into your focused window. Sanitising removes
  the keystrokes nobody speaks (and flattens newlines to spaces, so nothing is ever submitted),
  not the sentences you would not want typed.
- Speech that reads as an instruction to the cleanup model, from you or from anyone the
  microphone hears during a dictation, is content to clean, not a command: the built-in prompt
  says so and wraps the transcript in tags (`src/polish.rs`), and the polish bench measures it
  (`bench/polish_items.jsonl`, the `injection` stratum). A model that obeys anyway can only change
  the text typed into the window you focused, which you see before you send it: it has no tools
  and no memory between dictations, sanitising flattens newlines and removes control characters,
  so nothing is submitted, and a reply over `inject.max_chars` is held rather than typed.
- The process can see every keystroke while it runs, because that is how a global hotkey works.
- Anything running under your user account can reach the microphone, the IPC socket, the
  clipboard, the capture log and byovox's own memory. byovox draws no boundary against itself.

### Known limitations, accepted

Real, understood, and not being fixed — the cost of closing them exceeds what they buy:

- **The IPC socket can be denied, not driven.** Anything on the machine may open the pipe
  read-only and hold connection slots (255), or claim the name `byovox-<user>.sock` before
  byovox starts, which makes byovox refuse to start or lets the squatter answer the CLI. No
  transcript is exposed either way. Restricting the descriptor would close the first and not
  the second, and buys nothing against same-user processes, which dominate this boundary.
- **`INJECT_MARKER` is a public constant.** Another process can stamp it to hide its own
  synthetic keys from byovox's hotkey. That suppresses a hotkey and gains nothing, and any
  process able to call `SendInput` as you can already type anywhere. Hiding the constant
  would be obscurity in a binary anyone can disassemble.

## Hardening checklist

If any of the above matters to you:

- Point `stt.base_url`, every `stt.by_language.<code>.base_url` and `polish.base_url` at
  `localhost`, or reach a remote host over
  WireGuard/Tailscale, or use `https://` with a publicly-trusted certificate. `byovox check`
  tells you which of your endpoints are in clear.
- Leave `capture_log.enabled = false`. If you turn it on, keep `capture_log.keep_days` at a
  number you are comfortable with — `0` means for ever.
- Leave `logging.level = "info"`. `debug` writes your transcripts to disk.
- Prefer the `type` rung over `paste` if clipboard history or cloud clipboard sync is on.
- Review what your polish endpoint is: it sees every transcript before you do.
