//! Explicit live-server smoke test: one synthetic phrase, audio metadata only, no playback/files.
fn main() -> Result<(), String> {
    let base = std::env::args()
        .nth(1)
        .ok_or("usage: cargo run --example read_aloud_probe -- <TTS-base-url>")?;
    let cfg = byovox::read_aloud::TtsConfig {
        enabled: true,
        base_url: base,
        ..Default::default()
    };
    cfg.validate()?;
    let endpoint = cfg.endpoint("en")?;
    let audio = byovox::read_aloud::synthesize(&cfg, &endpoint, "Read aloud test.", || true)?;
    println!(
        "Validated live TTS audio: {} Hz, {} channel(s), {} samples",
        audio.rate,
        audio.channels,
        audio.samples.len()
    );
    Ok(())
}
