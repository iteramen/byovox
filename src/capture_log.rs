//! Opt-in per-dictation dump: `<dir>/<ts>-<n>.wav` plus a row in `<dir>/dictations.jsonl`; the only
//! place transcript text is written to disk. A failed write skips the row, so the JSONL never names a missing WAV.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::pipeline::{DictationRecord, Recorder};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The JSONL file; the index of what this log owns, the only thing that authorises deleting a WAV.
const ROWS: &str = "dictations.jsonl";

pub struct CaptureLog {
    dir: PathBuf,
    n: u64,
    /// `capture_log.keep_days`; `0` disables pruning entirely.
    keep_days: u32,
}

impl CaptureLog {
    pub fn new(dir: PathBuf, keep_days: u32) -> Result<CaptureLog> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let log = CaptureLog {
            dir,
            n: 0,
            keep_days,
        };
        log.sweep_stale_temps();
        log.prune();
        Ok(log)
    }

    /// Delete `dictations.jsonl.tmp-*` left by a prune that died before renaming; it holds transcripts.
    fn sweep_stale_temps(&self) {
        let prefix = format!("{ROWS}.tmp-");
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, path = %self.dir.display(), "capture log: could not scan for stale temp files");
                return;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(&prefix) {
                continue;
            }
            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    tracing::info!(path = %path.display(), "capture log: removed a temp file left by an interrupted prune")
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "capture log: could not remove a stale temp file")
                }
            }
        }
    }

    fn cutoff(&self) -> Option<u64> {
        if self.keep_days == 0 {
            return None;
        }
        Some(now_ms().saturating_sub(u64::from(self.keep_days) * DAY_MS))
    }

    /// Drop expired JSONL rows and delete the WAVs those rows name. The JSONL is the only source
    /// of deletion candidates, since a user's own `2024-1.wav` would pass a loose shape test.
    fn prune(&self) {
        let Some(cutoff) = self.cutoff() else { return };
        let path = self.dir.join(ROWS);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not read the rows");
                return;
            }
        };
        if text
            .lines()
            .next()
            .and_then(row_timestamp)
            .is_some_and(|ts| ts >= cutoff)
        {
            return;
        }
        // The file's order is load-bearing: the fast path trusts the first row to be the oldest.
        // A row whose `ts` cannot be read is kept, since this deletes data; one whose audio could
        // not be deleted is kept too, see `remove_capture`.
        let total = text.lines().count();
        let mut kept: Vec<&str> = Vec::with_capacity(total);
        for line in text.lines() {
            let expired = row_timestamp(line).is_some_and(|ts| ts < cutoff);
            if expired && self.remove_capture(line) {
                continue;
            }
            kept.push(line);
        }
        if kept.len() == total {
            return;
        }
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        // The pid keeps two byovox processes on one capture directory from clobbering each
        // other's temp file.
        let tmp = self.dir.join(format!("{ROWS}.tmp-{}", std::process::id()));
        if let Err(e) = replace_atomically(&path, &tmp, &body) {
            tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not rewrite the rows");
            // The original is still whole; the temp file is the only casualty.
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Delete the WAV one expired row names; `true` when the row may go (deleted, gone, or not a name this log wrote).
    /// Any other failure keeps the row, the only handle on its audio. The name is checked: `../../x` must not escape.
    fn remove_capture(&self, row: &str) -> bool {
        let Some(name) = serde_json::from_str::<serde_json::Value>(row)
            .ok()
            .and_then(|v| v.get("wav")?.as_str().map(str::to_owned))
            .filter(|n| is_own_capture_name(n))
        else {
            return true;
        };
        let path = self.dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not delete a wav; keeping its row to retry");
                false
            }
        }
    }
}

/// Replace `path`'s contents with `body` without a half-written window: sibling temp file (same volume), `sync_all`,
/// rename. `std::fs::write` truncates first; the handle is dropped before the rename because Windows refuses an open file.
fn replace_atomically(path: &Path, tmp: &Path, body: &str) -> std::io::Result<()> {
    {
        let mut f = std::fs::File::create(tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| capped(d.as_millis()))
        .unwrap_or(0)
}

/// Whether `name` is the exact shape `record` writes: thirteen digits of unix ms, `-`, a decimal counter, `.wav`.
/// The digit count matters: a looser test matches `2024-1.wav`, which a pruner would read as a 1970 timestamp.
fn is_own_capture_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".wav") else {
        return false;
    };
    let Some((ts, n)) = stem.split_once('-') else {
        return false;
    };
    ts.len() == 13
        && ts.bytes().all(|b| b.is_ascii_digit())
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
}

/// The `ts` of one JSONL row, or `None` when the line is not a row this log would recognise.
fn row_timestamp(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("ts")?
        .as_u64()
}

/// `serde_json::json!` panics on a `u128` past `u64::MAX`; saturating keeps `record` total.
fn capped(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

impl Recorder for CaptureLog {
    fn record(&mut self, r: &DictationRecord<'_>) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| capped(d.as_millis()))
            .unwrap_or(0);
        let wav_name = format!("{ts}-{}.wav", self.n);
        self.n += 1;
        let wav_path = self.dir.join(&wav_name);
        if let Err(e) = std::fs::write(&wav_path, r.audio.to_wav()) {
            tracing::error!(error = %e, path = %wav_path.display(), "capture log: writing wav failed");
            return;
        }
        let row = serde_json::json!({
            "ts": ts,
            "wav": wav_name,
            // The layout read at press time and the language of the request whose reply was kept.
            "layout": r.layout.map(|l| l.to_string()),
            "language": r.language.label(),
            // The language whisper detected on the auto-detect request and its probability.
            "detected": r.detected.map(|(code, _)| code),
            "detected_p": r.detected.map(|(_, p)| p),
            "raw": r.raw,
            // The strongest segment score; the gate decides on the weighted mean, not this.
            "no_speech_prob": r.no_speech_prob,
            "polished": r.polished,
            "polish_model": r.polish_model,
            "rung": r.rung,
            // Characters the sanitiser removed. With `rung: null` and a non-zero count, the reply
            // sanitised away to nothing and this row is the only record that it arrived.
            "dropped_chars": r.dropped_chars,
            "stt_ms": capped(r.stt_ms),
            "polish_ms": capped(r.polish_ms),
            "inject_ms": capped(r.inject_ms),
        });
        let path = self.dir.join(ROWS);
        // One `write_all`, newline included: `writeln!` on an unbuffered file streams fragments,
        // and a failure mid-row would leave a stub the next row is appended to.
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(format!("{row}\n").as_bytes()));
        if let Err(e) = appended {
            tracing::error!(error = %e, path = %path.display(), "capture log: appending row failed");
        }
        // After the write, so pruning only ever removes older rows.
        self.prune();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Audio;
    use crate::lang::{Lang, SttLanguage};
    use crate::pipeline::{DictationRecord, Recorder};

    fn temp_dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("byovox-capture-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn writes_wav_and_jsonl_row() {
        let dir = temp_dir("writes_wav_and_jsonl_row");
        let mut log = CaptureLog::new(dir.clone(), 30).unwrap();
        let audio = Audio {
            samples: vec![0; 1600],
        };
        let lang = SttLanguage::Explicit(Lang::parse("he").unwrap());
        log.record(&DictationRecord {
            audio: &audio,
            layout: Lang::parse("ru"),
            language: &lang,
            detected: Some(("he", 0.87)),
            raw: "raw\nline",
            no_speech_prob: Some(0.04),
            polished: Some("Polished."),
            polish_model: Some("cleanup-1"),
            rung: Some("type"),
            dropped_chars: 0,
            stt_ms: 600,
            polish_ms: 400,
            inject_ms: 10,
        });
        let jsonl = std::fs::read_to_string(dir.join("dictations.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(row["raw"], "raw\nline");
        assert_eq!(row["no_speech_prob"], 0.04_f32);
        assert_eq!(row["polished"], "Polished.");
        assert_eq!(row["layout"], "ru");
        assert_eq!(row["language"], "he");
        assert_eq!(row["detected"], "he");
        assert_eq!(row["detected_p"], 0.87_f32);
        assert_eq!(row["polish_model"], "cleanup-1");
        assert_eq!(row["rung"], "type");
        assert_eq!(row["stt_ms"], 600);
        assert_eq!(row["polish_ms"], 400);
        assert_eq!(row["inject_ms"], 10);
        assert!(row["ts"].is_number(), "ts: {}", row["ts"]);
        let wav = dir.join(row["wav"].as_str().unwrap());
        assert!(wav.exists(), "{}", wav.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_millisecond_dictations_keep_their_own_wav() {
        let dir = temp_dir("same_millisecond_dictations_keep_their_own_wav");
        let mut log = CaptureLog::new(dir.clone(), 30).unwrap();
        let audio = Audio {
            samples: vec![0; 16],
        };
        let lang = SttLanguage::Auto {
            candidates: vec![],
            prior: None,
        };
        for raw in ["one", "two"] {
            log.record(&DictationRecord {
                audio: &audio,
                layout: None,
                language: &lang,
                detected: None,
                raw,
                no_speech_prob: None,
                polished: None,
                polish_model: None,
                rung: None,
                dropped_chars: 0,
                stt_ms: 1,
                polish_ms: 0,
                inject_ms: 0,
            });
        }
        let jsonl = std::fs::read_to_string(dir.join("dictations.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        let wavs: Vec<&str> = rows.iter().map(|r| r["wav"].as_str().unwrap()).collect();
        assert_ne!(
            wavs[0], wavs[1],
            "one row would reference the other's audio"
        );
        for wav in wavs {
            assert!(dir.join(wav).exists(), "{wav}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn plant(dir: &Path, days_ago: u64, n: u64) -> String {
        let ts = now_ms() - days_ago * DAY_MS;
        let wav = format!("{ts}-{n}.wav");
        std::fs::write(dir.join(&wav), b"RIFFfake").unwrap();
        let row = serde_json::json!({"ts": ts, "wav": wav, "raw": "planted"});
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(ROWS))
            .unwrap();
        f.write_all(format!("{row}\n").as_bytes()).unwrap();
        wav
    }

    #[test]
    fn captures_past_keep_days_are_pruned_at_startup() {
        let dir = temp_dir("prune_at_startup");
        std::fs::create_dir_all(&dir).unwrap();
        let old = plant(&dir, 40, 0);
        let recent = plant(&dir, 3, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        assert!(!dir.join(&old).exists(), "a 40-day-old wav survived");
        assert!(dir.join(&recent).exists(), "a 3-day-old wav was deleted");
        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.contains(&recent), "{rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_days_zero_prunes_nothing() {
        let dir = temp_dir("prune_disabled");
        std::fs::create_dir_all(&dir).unwrap();
        let ancient = plant(&dir, 4_000, 0);

        let _log = CaptureLog::new(dir.clone(), 0).unwrap();

        assert!(dir.join(&ancient).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join(ROWS))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    const STRANGERS: [&str; 9] = [
        "2024-1.wav",
        "2019-12.wav",
        "1-1.wav",
        "0-0.wav",
        "20240101-1.wav",
        "notes.wav",
        "holiday-2019.wav",
        "2019.wav",
        "export.txt",
    ];

    #[test]
    fn pruning_never_touches_a_file_the_log_did_not_name() {
        let dir = temp_dir("prune_only_own_files");
        std::fs::create_dir_all(&dir).unwrap();
        plant(&dir, 90, 0);
        for stranger in STRANGERS {
            std::fs::write(dir.join(stranger), b"not mine").unwrap();
        }

        let _log = CaptureLog::new(dir.clone(), 1).unwrap();

        for stranger in STRANGERS {
            assert!(dir.join(stranger).exists(), "{stranger} was deleted");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wav_with_no_row_is_never_deleted() {
        let dir = temp_dir("prune_orphan_wav");
        std::fs::create_dir_all(&dir).unwrap();
        let orphan = "1700000000000-0.wav";
        std::fs::write(dir.join(orphan), b"RIFForphan").unwrap();
        let claimed = plant(&dir, 90, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        assert!(
            dir.join(orphan).exists(),
            "an unclaimed wav was deleted; only rows may name a deletion"
        );
        assert!(!dir.join(&claimed).exists(), "the claimed wav survived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_row_whose_wav_is_missing_drops_the_row_without_error() {
        let dir = temp_dir("prune_missing_wav");
        std::fs::create_dir_all(&dir).unwrap();
        let gone = plant(&dir, 90, 0);
        std::fs::remove_file(dir.join(&gone)).unwrap();
        let fresh = plant(&dir, 1, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.contains(&fresh), "{rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_wav_that_cannot_be_deleted_keeps_its_row_for_the_next_prune() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;

        let dir = temp_dir("prune_locked_wav");
        std::fs::create_dir_all(&dir).unwrap();
        let locked = plant(&dir, 90, 0);
        let fresh = plant(&dir, 1, 1);

        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(dir.join(&locked))
            .expect("hold the wav open");

        let log = crate::testutil::logged(|| {
            CaptureLog::new(dir.clone(), 30).unwrap();
        });

        assert!(
            dir.join(&locked).exists(),
            "the lock did not actually block the delete; the test proves nothing"
        );
        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(
            rows.lines().count(),
            2,
            "the row was dropped anyway: {rows}"
        );
        assert!(rows.contains(&locked), "{rows}");
        assert!(log.contains("keeping its row to retry"), "{log}");
        assert!(log.contains("os error 32"), "{log}");
        assert!(
            rows.lines().next().unwrap().contains(&locked),
            "time order broke; the fast path would never retry: {rows}"
        );

        drop(held);
        let log = CaptureLog::new(dir.clone(), 30).unwrap();
        let _ = log;
        assert!(!dir.join(&locked).exists(), "the retry did not delete it");
        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.contains(&fresh), "{rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_temp_file_from_an_interrupted_prune_is_swept_at_startup() {
        let dir = temp_dir("sweep_stale_temps");
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join(format!("{ROWS}.tmp-4242"));
        let ours = dir.join(format!("{ROWS}.tmp-{}", std::process::id()));
        std::fs::write(&stale, "{\"ts\":1,\"raw\":\"a transcript\"}\n").unwrap();
        std::fs::write(&ours, "{\"ts\":2,\"raw\":\"another\"}\n").unwrap();
        let neighbours = [
            format!("{ROWS}.bak"),
            format!("{ROWS}.tmp"),
            ROWS.to_string(),
            "notes.txt".to_string(),
        ];
        for n in &neighbours {
            std::fs::write(dir.join(n), b"keep me").unwrap();
        }

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        assert!(!stale.exists(), "a stale temp file survived");
        assert!(!ours.exists(), "debris from this pid's own crash survived");
        for n in &neighbours {
            assert!(dir.join(n).exists(), "{n} was deleted");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_row_naming_a_file_we_do_not_own_still_drops() {
        let dir = temp_dir("prune_unowned_name");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2024-1.wav"), b"not mine").unwrap();
        let ts = now_ms() - 90 * DAY_MS;
        let row = serde_json::json!({"ts": ts, "wav": "2024-1.wav", "raw": "x"});
        std::fs::write(dir.join(ROWS), format!("{row}\n")).unwrap();

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        assert!(dir.join("2024-1.wav").exists(), "a stranger was deleted");
        assert_eq!(std::fs::read_to_string(dir.join(ROWS)).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_row_naming_a_path_cannot_reach_outside_the_directory() {
        let dir = temp_dir("prune_traversal");
        let outside = dir.join("outside.wav");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(&outside, b"not a capture").unwrap();

        let inner = dir.join("inner");
        let ts = now_ms() - 90 * DAY_MS;
        for claim in ["../outside.wav", "..\\outside.wav", "/outside.wav"] {
            let row = serde_json::json!({"ts": ts, "wav": claim, "raw": "x"});
            std::fs::write(inner.join(ROWS), format!("{row}\n")).unwrap();
            let _log = CaptureLog::new(inner.clone(), 30).unwrap();
            assert!(outside.exists(), "`{claim}` reached outside the directory");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_timestamp_dash_counter_wav_is_ours() {
        assert!(is_own_capture_name("1700000000000-0.wav"));
        assert!(is_own_capture_name("1700000000000-12.wav"));
        assert!(is_own_capture_name(&format!("{}-0.wav", now_ms())));
        for other in [
            "2024-1.wav",
            "2019-12.wav",
            "1-1.wav",
            "0-0.wav",
            "20240101-1.wav",
            "17000000000000-0.wav",
            "notes.wav",
            "holiday-2019.wav",
            "2019.wav",
            "1700000000000-.wav",
            "-0.wav",
            "1700000000000-0.txt",
            "+700000000000-0.wav",
            "1700000000000- 0.wav",
            "../1700000000000-0.wav",
            "sub/1700000000000-0.wav",
            ROWS,
        ] {
            assert!(!is_own_capture_name(other), "{other} was claimed as ours");
        }
    }

    #[test]
    fn a_failed_rewrite_leaves_the_original_bytes_intact() {
        let dir = temp_dir("atomic_rewrite_failure");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ROWS);
        let original = "{\"ts\":1,\"wav\":\"a\"}\n{\"ts\":2,\"wav\":\"b\"}\n";
        std::fs::write(&path, original).unwrap();

        let doomed = dir.join("no-such-subdir").join("x.tmp");
        let err = replace_atomically(&path, &doomed, "replacement").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a failed rewrite must not have touched the original"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_rewrite_replaces_the_file_and_leaves_no_temp() {
        let dir = temp_dir("atomic_rewrite_success");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ROWS);
        std::fs::write(&path, "old\n").unwrap();
        let tmp = dir.join(format!("{ROWS}.tmp-{}", std::process::id()));

        replace_atomically(&path, &tmp, "new\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(!tmp.exists(), "the temp file survived the rename");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_prune_leaves_no_temp_file_behind() {
        let dir = temp_dir("prune_leaves_no_temp");
        std::fs::create_dir_all(&dir).unwrap();
        plant(&dir, 90, 0);
        plant(&dir, 1, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_row_is_kept_while_expired_ones_go() {
        let dir = temp_dir("prune_keeps_unreadable_rows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(ROWS), "{ this is not json\n").unwrap();
        plant(&dir, 90, 0);
        plant(&dir, 1, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert!(rows.contains("not json"), "the unreadable row went: {rows}");
        assert_eq!(rows.lines().count(), 2, "the 90-day row survived: {rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_prunes_what_startup_left_behind() {
        let dir = temp_dir("prune_after_write");
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = CaptureLog::new(dir.clone(), 30).unwrap();
        let old = plant(&dir, 60, 7);
        assert!(dir.join(&old).exists());

        let audio = Audio {
            samples: vec![0; 16],
        };
        let lang = SttLanguage::Auto {
            candidates: vec![],
            prior: None,
        };
        log.record(&DictationRecord {
            audio: &audio,
            layout: None,
            language: &lang,
            detected: None,
            raw: "fresh",
            no_speech_prob: None,
            polished: None,
            polish_model: None,
            rung: None,
            dropped_chars: 0,
            stt_ms: 1,
            polish_ms: 0,
            inject_ms: 0,
        });

        assert!(!dir.join(&old).exists(), "the write did not prune");
        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.contains("fresh"), "{rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
