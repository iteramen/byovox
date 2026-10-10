# Local speech server benchmarks

How fast and how accurate whisper.cpp models are as byovox's local speech server on a given
machine. Rounds are listed newest first. Each round's per-run numbers are committed beside it
in [`bench/`](bench/): timings, CPU load and scores only, with no transcript text. So a later
round can be compared run for run.

## 2026-09-28 — i7-8700K desktop, CPU only

**Machine:** Intel Core i7-8700K (6 cores, 12 threads), 24 GB RAM, Windows 11 Pro 26200, no
NVIDIA GPU. Nothing else was meant to run on it; runs taken while something did are listed
under Discarded runs.

**Server:** whisper.cpp `b5130` CPU build (`whisper-bin-x64.zip`, the build `byovox setup`
pins), started as `-m <model> --host 127.0.0.1 --port 8770 -l auto --inference-path
/v1/audio/transcriptions -t <threads>`. These are the arguments setup writes, plus `-t`.

**Models:** each file was checked against the Hugging Face size and SHA-256 at the revision
setup pins. The hashes are in the data file.

**Clips:** 15 in all:
- 1 s of digital silence, the probe `byovox setup` sends;
- 12 real dictations, four each of about 5, 10 and 17 s, mixed English and Russian;
- 2 Hebrew dictations of about 8 s, sent with `language=he` forced. Every other clip went
  with `language_candidates=en,ru` under the server's `-l auto`. The Hebrew clips ran faster,
  so their column is not comparable with the others.

**Method:**
- Each request is a multipart POST like byovox's, with `response_format=verbose_json`.
- Wall time is measured from the request to the full reply. The model load and one warm-up
  request are excluded.
- Each clip runs 3 times per cell, and a cell's time for a clip is the median of the 3.
- Before each run, CPU load is sampled every second for 10 s. The run starts only below a 10 %
  mean, with no `cargo`, `rustc`, `link` or other `whisper-server` process running.
- During each run, load is sampled every second and the server's own share is subtracted. A
  run is discarded and repeated when the rest of the machine averaged over 10 % or peaked
  over 25 %. 9 of 594 runs were discarded this way (listed below).

**Accuracy:** character and word error rates against each clip's reference transcript. The
reference is what whisper `large-v3` on a GPU server returned for the clip, so these rates measure agreement with large-v3, not with what was said. Both sides are
Unicode-normalised (NFKC), lowercased, stripped of punctuation and symbols, and have their
whitespace collapsed. Scores are from the first run at `-t 4`.

### Time per request (seconds)

For each clip-length bucket: the mean over its clips of each clip's median of 3, then the
slowest clip's median in brackets. "× timeout" is the slowest speech clip against byovox's
default `stt.timeout_s = 30`.

| model | `-t` | silence 1 s | ~5 s | ~10 s | ~17 s | he ~8 s, forced | × timeout |
|---|---|---|---|---|---|---|---|
| base | 4 | 2.82 | 2.59 (2.67) | 2.64 (2.71) | 2.70 (2.75) | 2.20 (2.49) | 0.09 |
| base | 6 | 2.16 | 2.03 (2.23) | 2.02 (2.19) | 2.03 (2.08) | 1.72 (1.98) | 0.07 |
| base-q8_0 | 4 | 2.27 | 2.10 (2.15) | 2.12 (2.16) | 2.17 (2.19) | 2.05 (2.55) | 0.09 |
| base-q8_0 | 6 | 1.85 | 1.66 (1.88) | 1.61 (1.66) | 1.63 (1.66) | 1.65 (2.16) | 0.07 |
| small-q5_1 | 4 | 12.33 | 11.43 (12.72) | 10.66 (10.73) | 10.74 (10.77) | 7.32 (7.33) | 0.42 |
| small-q5_1 | 6 | 9.26 | 8.56 (9.58) | 7.78 (7.87) | 7.81 (7.90) | 5.35 (5.37) | 0.32 |
| small-q8_0 | 4 | 8.24 | 7.52 (7.89) | 7.45 (7.55) | 7.51 (7.59) | 5.21 (5.24) | 0.26 |
| small-q8_0 | 6 | 6.31 | 5.90 (6.60) | 5.49 (5.57) | 5.52 (5.57) | 3.84 (3.85) | 0.22 |
| small-q8_0 | 12 | 4.43 | 4.55 (4.58) | 4.72 (4.83) | 4.79 (4.90) | 3.35 (3.39) | 0.16 |
| small | 4 | 10.41 | 9.56 (10.15) | 9.84 (10.44) | 9.59 (9.73) | 6.65 (6.66) | 0.35 |
| small | 6 | 8.11 | 7.47 (8.24) | 6.94 (7.12) | 7.06 (7.17) | 4.98 (5.01) | 0.27 |
| large-v3-turbo-q5_0 | 4 | 63.04 | 56.79 (59.34) | 54.55 (54.61) | 54.55 (54.62) | 36.55 (36.56) | **1.98** |
| large-v3-turbo-q5_0 | 6 | 46.66 | 43.06 (47.61) | 39.02 (39.51) | 39.08 (39.16) | 26.21 (26.34) | **1.59** |

Time barely depends on clip length: whisper encodes a 30 s window whatever the clip's length.

### Agreement with large-v3 (CER / WER)

| model | English (6 clips) | Russian (6) | Hebrew (2) |
|---|---|---|---|
| base | 8.0 % / 13.7 % | 6.0 % / 23.8 % | 55.2 % / 93.3 % |
| base-q8_0 | 8.9 % / 16.0 % | 5.8 % / 22.5 % | 55.2 % / 80.0 % |
| small-q5_1 | 2.7 % / 6.1 % | 4.4 % / 13.8 % | 27.6 % / 63.3 % |
| small-q8_0 | 2.5 % / 5.3 % | 3.8 % / 11.3 % | 23.4 % / 60.0 % |
| small | 2.7 % / 6.1 % | 3.8 % / 11.3 % | 23.4 % / 60.0 % |
| large-v3-turbo-q5_0 | 1.0 % / 2.3 % | 2.9 % / 6.3 % | 11.0 % / 33.3 % |

Every model detected the language of all 12 English and Russian clips correctly, except
base-q8_0, which got 11 of 12 on its first run at `-t 4` (12 of 12 at `-t 6`).

### 1 s of digital silence

| model | text | `no_speech_prob` |
|---|---|---|
| base, base-q8_0 | `[BLANK_AUDIO]` | 0.90 |
| small-q5_1, small-q8_0, small | `[BLANK_AUDIO]` | 0.93–0.94 |
| large-v3-turbo-q5_0 | `Thank you.` | 1.6e-10 |

byovox's no-speech gate drops a dictation scoring above `stt.no_speech_threshold` (0.3), so
silence sent to any of these models but turbo-q5_0 types nothing. Turbo returns a stock
phrase that the gate passes. byovox itself never sends digital silence from a dictation.

### Discarded runs

9 runs were repeated because the rest of the machine was busy. Each is kept in the data file
with `discarded: true`.

| model | `-t` | clip | repeat | background mean / peak |
|---|---|---|---|---|
| base | 6 | c07 | 3 | 17.0 % / 25.2 % |
| base | 6 | c07 | 3 (again) | 15.8 % / 18.9 % |
| base | 6 | c10 | 1 | 37.8 % / 50.8 % |
| base | 6 | c12 | 3 | 11.5 % / 17.5 % |
| base-q8_0 | 4 | c07 | 2 | 13.2 % / 20.5 % |
| base-q8_0 | 6 | c08 | 2 | 13.9 % / 20.0 % |
| small-q8_0 | 6 | c06 | 1 | 10.9 % / 17.7 % |
| small | 6 | c11 | 1 | 28.0 % / 45.2 % |
| large-v3-turbo-q5_0 | 4 | c13 | 3 | 8.3 % / 25.1 % |

Data: [`bench/2026-09-28-i7-8700k.json`](bench/2026-09-28-i7-8700k.json).
