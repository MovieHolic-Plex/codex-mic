// Which single-key strings parse into registrable shortcuts?
fn main() {
    for s in [
        "F9", "F13", "CapsLock", "AltRight", "RAlt", "Alt", "ShiftRight",
        "Backquote", "`", "Space", "Ctrl+E", "Ctrl+F9", "ScrollLock", "Pause",
        "NumLock", "Insert", "Home",
    ] {
        match s.parse::<tauri_plugin_global_shortcut::Shortcut>() {
            Ok(sc) => println!("OK   {s:12} -> {sc:?}"),
            Err(e) => println!("FAIL {s:12} -> {e}"),
        }
    }
}
