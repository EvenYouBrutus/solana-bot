use crate::config::types::Config;
use std::{fs, path::Path};
use thiserror::Error;

pub mod types;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration: {0}")]
    Read(#[from] std::io::Error),
    #[error("invalid TOML configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(&fs::read_to_string(path)?)?;
    config.validate()?;
    Ok(config)
}
