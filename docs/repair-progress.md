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
