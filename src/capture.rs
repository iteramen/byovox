//! Microphone capture trait: open on press, closed on release, never held open.

use std::sync::mpsc::{RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::Audio;

pub trait Capture: Send {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self) -> Result<Audio, String>;
}

const READY_TIMEOUT: Duration = Duration::from_secs(5);

const RESERVE_SECS: usize = 30;

/// One recording's entire mutable state, allocated fresh by `start()` and dropped by `stop()`, so
/// a thread abandoned by a timed-out `start()` writes only into Arcs nobody reads.
struct Recording {
    stop_tx: Sender<()>,
    handle: JoinHandle<Result<(), String>>,
    buffer: Arc<Mutex<Vec<f32>>>,
    /// First error the stream reported, if any; a recording that hit one is failed, not
    /// silently returned truncated.
    error: Arc<Mutex<Option<String>>>,
}

pub struct CpalCapture {
    device: String,
    /// The layout the *running* stream opened with, overwritten by `start()`: WASAPI re-reads
    /// `GetMixFormat` on every query, so a format change would otherwise leave `stop()`
    /// resampling at a stale rate.
    channels: u16,
    rate: u32,
    worker: Option<Recording>,
}

impl CpalCapture {
    pub fn open(selector: &str) -> Result<CpalCapture, String> {
        let (_device, cfg) = input_device(selector)?;
        Ok(CpalCapture {
            device: selector.to_string(),
            channels: cfg.channels(),
            rate: cfg.sample_rate(),
            worker: None,
        })
    }
}

fn input_device(selector: &str) -> Result<(cpal::Device, cpal::SupportedStreamConfig), String> {
    let device = match selector.trim() {
        "" => cpal::default_host()
            .default_input_device()
            .ok_or("no default input device")?,
        wanted => {
            let mut devices = input_devices()?;
            let chosen = resolve(wanted, &names(&devices))?;
            devices.swap_remove(chosen)
        }
    };
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    validate(cfg.sample_rate(), cfg.channels(), cfg.sample_format())
        .map_err(|e| format!("{}: {e}", device_name(&device)))?;
    Ok((device, cfg))
}

fn input_devices() -> Result<Vec<cpal::Device>, String> {
    cpal::default_host()
        .input_devices()
        .map(|d| d.collect())
        .map_err(|e| format!("listing input devices: {e}"))
}

fn names(devices: &[cpal::Device]) -> Vec<String> {
    devices.iter().map(device_name).collect()
}

pub fn input_names() -> Result<Vec<String>, String> {
    Ok(names(&input_devices()?))
}

fn select(selector: &str, names: &[String]) -> Result<usize, String> {
    let wanted = selector.trim().to_lowercase();
    // Every substring contains the empty one, so an empty selector would silently pick the first
    // device; the default is never chosen from a list.
    if wanted.is_empty() {
        return Err("capture.device is empty: that is the system default, not a name".into());
    }
    let names: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    names
        .iter()
        .position(|n| *n == wanted)
        .or_else(|| names.iter().position(|n| n.contains(&wanted)))
        .ok_or_else(|| {
            format!(
                "capture.device {:?} matched no input device",
                selector.trim()
            )
        })
}

fn resolve(selector: &str, names: &[String]) -> Result<usize, String> {
    select(selector, names).map_err(|e| {
        if names.is_empty() {
            format!("{e}; no input devices found")
        } else {
            format!("{e}; available: {}", names.join(" | "))
        }
    })
}

pub fn validate_selector(selector: &str) -> Result<(), String> {
    if selector.trim().is_empty() {
        return Ok(());
    }
    resolve(selector, &input_names()?).map(|_| ())
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unnamed".into())
}

/// `Audio::from_f32` reads a zero rate as "already at target, leave it alone" and clamps a
/// zero channel count to mono, so a device reporting either would yield silently-wrong audio
/// rather than an error. Reject it at the boundary instead.
fn validate_layout(rate: u32, channels: u16) -> Result<(), String> {
    if rate == 0 {
        return Err(format!("device reports sample rate {rate}"));
    }
    if channels == 0 {
        return Err(format!("device reports {channels} channels"));
    }
    Ok(())
}

fn validate(rate: u32, channels: u16, format: cpal::SampleFormat) -> Result<(), String> {
    validate_layout(rate, channels)?;
    if !matches!(
        format,
        cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
    ) {
        return Err(format!("unsupported sample format {format:?}"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub format: cpal::SampleFormat,
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} Hz, {} ch, {:?})",
            self.name, self.rate, self.channels, self.format
        )
    }
}

pub fn describe_device(selector: &str) -> Result<DeviceInfo, String> {
    let (device, cfg) = input_device(selector)?;
    Ok(DeviceInfo {
        name: device_name(&device),
        rate: cfg.sample_rate(),
        channels: cfg.channels(),
        format: cfg.sample_format(),
    })
}

impl Capture for CpalCapture {
    fn start(&mut self) -> Result<(), String> {
        if self.worker.is_some() {
            return Err("already recording".into());
        }
        let buffer = Arc::new(Mutex::new(Vec::with_capacity(
            RESERVE_SECS * self.rate as usize * self.channels.max(1) as usize,
        )));
        let error = Arc::new(Mutex::new(None));
        let (thread_buffer, thread_error) = (buffer.clone(), error.clone());
        let (stop_tx, stop_rx) = channel::<()>();
        let (ready_tx, ready_rx) = channel::<Result<(u32, u16), String>>();
        let selector = self.device.clone();
        let handle = std::thread::Builder::new()
            .name("byovox-capture".into())
            .spawn(move || {
                let built = build_stream(&selector, thread_buffer, thread_error);
                let (stream, rate, channels) = match built {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.clone()));
                        return Err(e);
                    }
                };
                // WASAPI init can outlast start()'s deadline, which has then dropped stop_tx: release
                // the device unplayed so an abandoned recording never puts a live microphone out there.
                if !should_play(stop_rx.try_recv()) {
                    drop(stream);
                    return Ok(());
                }
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return Err(e.to_string());
                }
                let _ = ready_tx.send(Ok((rate, channels)));
                let _ = stop_rx.recv(); // blocks until stop(), or until the sender drops
                drop(stream);
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        let (rate, channels) = match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(ready) => ready?,
            // Abandon: dropping stop_tx closes stop_rx, which the thread checks before playing, so a
            // straggler is dropped unplayed and touches only this recording's own buffer and error slot.
            Err(RecvTimeoutError::Timeout) => {
                drop(stop_tx);
                return Err("microphone did not start within 5 s".into());
            }
            Err(RecvTimeoutError::Disconnected) => return Err("capture thread died".into()),
        };
        if let Err(e) = validate_layout(rate, channels) {
            drop(stop_tx);
            return Err(e);
        }
        self.rate = rate;
        self.channels = channels;
        self.worker = Some(Recording {
            stop_tx,
            handle,
            buffer,
            error,
        });
        Ok(())
    }

    fn stop(&mut self) -> Result<Audio, String> {
        let Some(rec) = self.worker.take() else {
            return Err("not recording".into());
        };
        let _ = rec.stop_tx.send(());
        rec.handle
            .join()
            .map_err(|_| "capture thread panicked".to_string())??;
        let samples = std::mem::take(&mut *rec.buffer.lock().unwrap());
        if let Some(e) = rec.error.lock().unwrap().take() {
            return Err(format!("capture stream error: {e}"));
        }
        Ok(Audio::from_f32(&samples, self.channels, self.rate)
            .without_start_click()
            .without_window_tail())
    }
}

/// Whether the capture thread should play the stream it just built. Only an empty channel means
/// `start()` is still waiting: `Disconnected` is a timed-out `start()`, `Ok(())` a stop that beat
/// the stream up; either way the microphone must not go live.
fn should_play(signal: Result<(), TryRecvError>) -> bool {
    matches!(signal, Err(TryRecvError::Empty))
}

fn build_stream(
    selector: &str,
    buffer: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<(cpal::Stream, u32, u16), String> {
    let (device, cfg) = input_device(selector)?;
    let (rate, channels) = (cfg.sample_rate(), cfg.channels());
    let err_fn = move |e: cpal::StreamError| {
        tracing::error!(error = %e, "capture stream error");
        let mut slot = error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(e.to_string());
        }
    };
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &cfg.into(),
            move |data: &[f32], _| buffer.lock().unwrap().extend_from_slice(data),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &cfg.into(),
            move |data: &[i16], _| {
                buffer
                    .lock()
                    .unwrap()
                    .extend(data.iter().map(|s| *s as f32 / i16::MAX as f32))
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &cfg.into(),
            move |data: &[u16], _| {
                buffer
                    .lock()
                    .unwrap()
                    .extend(data.iter().map(|s| (*s as f32 - 32768.0) / 32768.0))
            },
            err_fn,
            None,
        ),
        other => return Err(format!("unsupported sample format {other:?}")),
    };
    stream
        .map(|s| (s, rate, channels))
        .map_err(|e| format!("building input stream: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{
        Capture, CpalCapture, DeviceInfo, TryRecvError, resolve, select, should_play, validate,
        validate_layout,
    };
    use cpal::SampleFormat;

    fn devices() -> Vec<String> {
        [
            "Microphone Array (Example Audio)",
            "Headset (Example Buds Hands-Free)",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn a_zero_rate_or_channel_count_is_rejected() {
        assert!(validate_layout(48_000, 2).is_ok());
        assert!(
            validate_layout(0, 2).unwrap_err().contains("sample rate 0"),
            "{:?}",
            validate_layout(0, 2)
        );
        assert!(
            validate_layout(48_000, 0)
                .unwrap_err()
                .contains("0 channels"),
            "{:?}",
            validate_layout(48_000, 0)
        );
    }

    #[test]
    fn only_the_decodable_sample_formats_are_accepted() {
        for f in [SampleFormat::F32, SampleFormat::I16, SampleFormat::U16] {
            assert!(validate(48_000, 2, f).is_ok(), "{f:?}");
        }
        for f in [
            SampleFormat::I8,
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::U8,
            SampleFormat::U32,
            SampleFormat::F64,
        ] {
            let e = validate(48_000, 2, f).unwrap_err();
            assert!(e.contains("unsupported sample format"), "{f:?}: {e}");
            assert!(e.contains(&format!("{f:?}")), "{f:?}: {e}");
        }
    }

    #[test]
    fn a_bad_layout_is_reported_before_the_format() {
        let e = validate(0, 2, SampleFormat::F64).unwrap_err();
        assert!(e.contains("sample rate 0"), "{e}");
    }

    #[test]
    fn a_built_stream_is_played_only_while_start_is_still_waiting() {
        assert!(should_play(Err(TryRecvError::Empty)), "start still waiting");
        assert!(
            !should_play(Err(TryRecvError::Disconnected)),
            "start timed out and abandoned us"
        );
        assert!(!should_play(Ok(())), "a stop beat the stream up");
    }

    #[test]
    fn stop_before_any_start_is_an_error() {
        let mut cap = CpalCapture {
            device: String::new(),
            channels: 2,
            rate: 48_000,
            worker: None,
        };
        assert_eq!(cap.stop().unwrap_err(), "not recording");
    }

    #[test]
    fn a_selector_matches_any_part_of_a_device_name_in_any_case() {
        let d = devices();
        assert_eq!(select("Microphone Array", &d), Ok(0));
        assert_eq!(select("microphone array", &d), Ok(0));
        assert_eq!(select("EXAMPLE AUDIO", &d), Ok(0));
        assert_eq!(select("Hands-Free", &d), Ok(1));
        assert_eq!(select("  Headset  ", &d), Ok(1));
    }

    #[test]
    fn an_exact_name_wins_and_otherwise_the_first_match() {
        let d = vec!["Microphone Array".to_string(), "Microphone".to_string()];
        assert_eq!(select("Microphone", &d), Ok(1), "the exact name");
        assert_eq!(
            select(" microphone ", &d),
            Ok(1),
            "the exact name, in any case"
        );
        assert_eq!(select("Micro", &d), Ok(0), "a part: the first match");
        assert_eq!(select("Microphone Array", &d), Ok(0));
    }

    #[test]
    fn a_selector_that_matches_nothing_names_the_key_the_value_and_the_alternatives() {
        let e = select("nonexistent", &devices()).unwrap_err();
        assert_eq!(e, "capture.device \"nonexistent\" matched no input device");
        assert_eq!(
            resolve("nonexistent", &devices()).unwrap_err(),
            "capture.device \"nonexistent\" matched no input device; available: \
             Microphone Array (Example Audio) | Headset (Example Buds Hands-Free)"
        );
        assert_eq!(resolve("Headset", &devices()), Ok(1));
        assert!(select("", &devices()).is_err());
        assert!(select("   ", &devices()).is_err());
        assert!(select("Microphone Array", &[]).is_err());
    }

    #[test]
    fn a_device_describes_itself_with_the_layout_it_reports() {
        let info = DeviceInfo {
            name: "Headset (Example Buds Hands-Free)".into(),
            rate: 16_000,
            channels: 1,
            format: SampleFormat::F32,
        };
        assert_eq!(
            info.to_string(),
            "Headset (Example Buds Hands-Free) (16000 Hz, 1 ch, F32)"
        );
    }
}
