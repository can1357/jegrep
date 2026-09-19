//! Locate provider API keys: process env first, then `~/.env`.

use std::{env, fs, path::PathBuf};

pub fn api_key(key_name: &str) -> Result<String, String> {
    if let Ok(k) = env::var(key_name)
        && !k.trim().is_empty() {
            return Ok(k.trim().to_string());
        }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let path = home.join(".env");
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key_name {
            continue;
        }
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
        if !v.is_empty() {
            return Ok(v.to_string());
        }
    }
    Err(format!(
        "{key_name} not found in the environment or in {}",
        path.display()
    ))
}
