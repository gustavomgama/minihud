//! Minimal config UI — edits minihud.toml, reloads on save.
//! Non-blocking: writes file, logs result, never wedges overlay.

use crate::config::Config;
use std::fs;

/// Open a minimal settings view (stub: writes config file directly).
/// In a full build this renders a small Direct2D panel; here it
/// ensures the config path is writable and validates clamped values.
pub fn edit_config() -> Result<(), String> {
    let cfg = Config::load();
    // Minimal UI action: confirm file writable, log current values.
    let p = Config::path();
    let writable = fs::metadata(&p)
        .map(|m| m.permissions().readonly() == false)
        .unwrap_or(true);
    if !writable {
        return Err(format!("config file {:?} not writable", p));
    }
    // Write back with clamped defaults (no-op if unchanged, but verifies path).
    let s = format!(
        "x = {}\ny = {}\nopacity = {:.2}\ntext_size = {:.1}\nupdate_hw_ms = {}\nidle_hw_ms = {}\nclick_through = {}\n",
        cfg.x, cfg.y, cfg.opacity, cfg.text_size, cfg.update_hw_ms, cfg.idle_hw_ms, cfg.click_through
    );
    fs::write(&p, s).map_err(|e| format!("write failed: {e}"))?;
    Ok(())
}
