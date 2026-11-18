//! Input-device metadata only. Does not open a recording stream or contact an endpoint.
fn main() -> Result<(), String> {
    let cfg = byovox::config::load(&byovox::config::default_path())
        .map_err(|_| "Configuration could not be loaded".to_owned())?;
    println!("Configured selector: {:?}", cfg.capture.device);
    let selected = byovox::capture::describe_device(&cfg.capture.device)?;
    println!("Selected input: {selected}");
    for name in byovox::capture::input_names()? {
        println!("Available input: {name:?}");
    }
    Ok(())
}
