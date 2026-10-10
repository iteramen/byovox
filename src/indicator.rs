//! Recording indication: tray colour always; pill and cue as configured layers.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndicatorState {
    Idle,
    Recording,
    Working,
    /// Painted like Idle; the only state that plays the done cue.
    Done,
    /// Done, but a segment's `no_speech_prob` exceeded `stt.no_speech_warn`.
    Uncertain,
    /// Shown for ~3 s by the UI, then Idle; the pipeline never sets Idle after Error.
    Error,
}

pub trait Indicator: Send {
    fn set(&mut self, state: IndicatorState);
}
