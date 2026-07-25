use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    let default_in = host.default_input_device().and_then(|d| d.name().ok());
    let default_out = host.default_output_device().and_then(|d| d.name().ok());
    println!("DEFAULT INPUT : {default_in:?}");
    println!("DEFAULT OUTPUT: {default_out:?}");
    println!("--- all input devices ---");
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            let name = d.name().unwrap_or_default();
            let cfg = d.default_input_config().ok();
            let desc = cfg.map(|c| format!("{}ch {}Hz {:?}", c.channels(), c.sample_rate().0, c.sample_format())).unwrap_or_default();
            let mark = if Some(&name) == default_in.as_ref() { " <-- DEFAULT" } else { "" };
            println!("  {name}  [{desc}]{mark}");
        }
    }
}
