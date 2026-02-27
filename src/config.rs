use std::path::PathBuf;

use anyhow::Context;

#[derive(Clone, Debug)]
pub struct OuijaConfig {
    pub name: String,
    pub port: u16,
    pub data_dir: PathBuf,
    /// Nostr public key used as the daemon's universal identity.
    pub npub: String,
}

impl OuijaConfig {
    pub fn default_data_dir() -> PathBuf {
        dirs_data_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    pub fn new(
        name: String,
        port: u16,
        data_dir: Option<String>,
        npub: String,
    ) -> anyhow::Result<Self> {
        let data_dir = match data_dir {
            Some(d) => PathBuf::from(d),
            None => dirs_data_dir()?,
        };
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("creating data dir: {}", data_dir.display()))?;
        Ok(Self {
            name,
            port,
            data_dir,
            npub,
        })
    }
}

fn dirs_data_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".local/share")
        });
    Ok(base.join("ouija"))
}
