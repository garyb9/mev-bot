//! Persisted, human-editable watchlist (SPEC-0001 §6).
//!
//! Format is newline-delimited; blank lines and `#` comments are ignored, so
//! the file is safe to edit by hand. This is a config object, not trading
//! data, so it lives in a plain file rather than SQLite.

use std::path::Path;

use crate::error::{Error, Result};

/// Default watchlist location relative to the working directory.
pub const DEFAULT_PATH: &str = "data/watchlist.txt";

/// Parse watchlist text, trimming entries and skipping blanks/comments.
pub fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Load the watchlist, returning an empty list if the file does not exist.
pub fn load(path: &Path) -> Result<Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(Error::Config(format!(
            "reading watchlist {}: {err}",
            path.display()
        ))),
    }
}

/// Persist the watchlist, creating parent directories as needed.
pub fn save(path: &Path, coins: &[String]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| Error::Config(format!("creating {}: {err}", parent.display())))?;
    }
    let body: String = coins.iter().map(|coin| format!("{coin}\n")).collect();
    std::fs::write(path, body)
        .map_err(|err| Error::Config(format!("writing watchlist {}: {err}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_blanks_and_comments() {
        let text = "# bluechips\nBTC\n\n  ETH  \n# xyz:TSLA\n";
        assert_eq!(parse(text), vec!["BTC", "ETH"]);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watchlist.txt");
        let coins = vec!["BTC".to_string(), "xyz:TSLA".to_string()];
        save(&path, &coins).unwrap();
        assert_eq!(load(&path).unwrap(), coins);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.txt");
        assert!(load(&path).unwrap().is_empty());
    }
}
