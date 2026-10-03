//! `hl config show`: print the resolved config (secrets redacted).

use crate::*;

pub(crate) fn show_config() -> Result<()> {
    let config = Config::load(ConfigOverrides::default())?;
    println!("{}", config.summary());
    Ok(())
}
