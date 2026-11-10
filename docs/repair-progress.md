# Reliability review repair queue — 2026-11-10

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
