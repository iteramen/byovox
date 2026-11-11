# Reliability review repair queue â€” 2026-11-10

Owner direction: repair one item at a time, preserve Git history and validation evidence.
Each item uses an isolated branch and PR. Merge and installation are separate milestones;
repository policy stops at PR. No version bump or deployment is part of item 1.

## Verified baseline

- Phone: signed Voxdroid 0.3.0, code 5, APK matches published SHA256; basic dictation passes.
- Desktop: byovox narration branch 5745834; separate Ctrl+Shift+Z dictation passes,
  Ctrl+Alt+R narration fails in Notepad before TTS (no selection state).
- Voxdroid narration 502d129: isolated handset checks silently cancel before completion;
  exact cancellation source remains unproven. Test app removed, production service restored.
- Passing CI did not establish runtime acceptance.

## Ordered work

1. **In progress: incomplete cleanup responses (both apps).** Reject explicit non-stop
   finish reasons, preserve the full raw transcript through existing fallback and notice.
   Keep compatibility with existing servers that omit finish_reason. Explicit null is not
   a completed response. Tests must fail before the parser fix and pass afterwards.
2. Queued: cancel active Android dictation transport so new takes do not wait behind it.
3. Queued: make separate byovox shortcuts the product default; keep selection discovery
   from delaying recording stop/cancel. Diagnose the actual Notepad provider failure.
4. Queued: qualify/fix Android narration event cancellation and automatic screen timeout.
5. Queued: restore an accessible tap trigger on Android's own Try screen.
6. Queued: reconcile Android TTS endpoint validation and cancellation with security policy;
   keep security reproduction details local until reviewed.
7. Queued: reassess documented production error-body logging.

## Item 1 contract

A successful HTTP response may still contain partial output. If the first choice includes
finish_reason, only the string stop is accepted. Other values (including length, filtering,
tool calls, unknown strings, null and malformed types) produce a fixed content-free error.
Missing finish_reason preserves the existing compatible-server behavior. This cannot detect
an endpoint silently truncating output while omitting or misreporting its completion reason.
The existing output token cap and cleanup prompt remain unchanged.

## Validation history

- Regression cases added for incomplete and normal stop responses using loopback servers.
- Red/green results and PR links will be recorded before handoff.

Tracking: https://github.com/iteramen/byovox/issues/35; paired app: https://github.com/iteramen/voxdroid/issues/48.

- RED: loopback returned partial content with finish_reason=length; old parser accepted it.
- Regression commit precedes implementation in this branch.

- GREEN (desktop): fmt, clippy, full tests (438 library + 5 CLI passed; 18 ignored), and benchmark self-test passed. The existing pipeline raw-fallback regression also passed.
- Implementation complete locally; CI and PR review pending. Installed binaries are unchanged.

## Standing owner ruling and item 3 (2026-11-10)

Owner authorized continuing the queue autonomously and merging each repair on green CI.
Pause only for a real blocker, owner decision, or required manual test. Keep feature PRs
unmerged while their runtime acceptance remains failed. Corrections to an unmerged feature
are reviewed as stacked PRs into that feature branch.

Item 1 merged: Android #49 / 46c97b6, desktop #36 / 760b5e0.
Item 2 merged: Android #51 / 281ab39; native transport header/body probes passed.
Item 3 in progress: separate narration default/binding validation; asynchronous, bounded
selection lookup so dictation release/cancel cannot wait behind Windows accessibility.
Notepad selection failure remains a separate native diagnostic checkpoint after this repair.

Item 3 issue: https://github.com/iteramen/byovox/issues/37.
The default-binding regression failed on the old shared default. Local fmt/clippy/full tests
passed after repair (460 library + 5 CLI; 18 ignored). Dedicated selection worker and pipeline
admission checks are covered; native Notepad behavior still requires owner-operated verification.
This repair PR targets feat/read-aloud; parent #33 must remain draft until native acceptance.


### Item 3 native follow-up: #39

Repair #38 merged as bad24b1 after all four CI checks passed. Installed reviewed head af15486
with owner-approved Synaptics array and restored prior TTS configuration; other settings and
startup entry verified unchanged, rollback copies retained. Owner retried both applications and
reported failure: focused role 50033, visible providers 0. Parent #33 remains draft.

Follow-up #39 isolates the selection client's COM lifecycle: a native regression failed on the
borrowed audio STA; a dedicated persistent MTA client replaces it without changing dictation.
Code/CI proof and real-app selection acceptance remain separate checkpoints.

Local validation: fmt, clippy, 463 library + 5 CLI tests passed (18 ignored); benchmark self-test passed. Three native COM lifecycle tests pass. Native app acceptance remains pending.


### Owner reprioritization: everyday-app coverage (#42)

b8e6bac passed Windows Notepad narration and second-press Stop. Notepad++ and the chat client
still need coverage. Owner requires practical cross-app use and approved guarded Copy fallback
by default, ahead of further Android work. Keep separate shortcuts and the Synaptics array.
The default/opt-out regression failed before implementation (ad27ca7). Clipboard privacy/security
contract is updated with restoration, stale-data guards, source-defined Copy behavior and limits.
Native coverage remains pending; parent #33 remains draft. Android main fixes were synchronized
into its narration branch (0ec9d0d), but item 4 implementation has not started.

Copy candidate local validation: fmt/clippy, 469 library + 5 CLI tests passed (18 ignored), benchmark self-test passed. Native private-station clipboard round-trip/race tests passed without touching the interactive clipboard. Application-level Copy acceptance is still pending.


### #42 terminal follow-up

PR #43 merged a2b18cc; installed reviewed 55acb02 with configuration unchanged. Owner confirmed
Notepad++ works; chat still failed. Status identifies the deliberate terminal refusal. Add the
Windows Terminal-specific Ctrl+Insert Copy route, keeping other terminal hosts refused and
retaining clipboard/source/generation guards. Regression 9bdb155 failed on both route and keys.
No terminal/chat native acceptance is claimed before the new candidate is tested.

Terminal candidate local validation: fmt/clippy, 471 library + 5 CLI tests passed (18 ignored), benchmark self-test passed. Native clipboard tests still pass. Terminal selected-text/empty-selection behavior remains a manual qualification checkpoint.
