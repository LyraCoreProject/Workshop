use anyhow::{Context, bail};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

pub const TOKEN_VARIABLE: &str = "WORKSHOP_GITHUB_TOKEN";
const MINIMUM_REFRESH: u64 = 60;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repository: String,
    #[serde(default)]
    pub maintainers: Vec<String>,
    pub listen: SocketAddr,
    /// What a contributor pastes into an agent started in a clone of the repository.
    pub start_prompt: String,
    #[serde(default = "default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
}

fn default_refresh_interval_secs() -> u64 {
    900
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid configuration in {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Config = toml::from_str(text)?;
        let parts: Vec<&str> = config.repository.split('/').collect();
        let valid_part = |part: &str| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if parts.len() != 2 || !parts.iter().all(|part| valid_part(part)) {
            bail!("repository must be owner/name, got {:?}", config.repository);
        }
        if let Some(login) = config.maintainers.iter().find(|login| {
            login.is_empty() || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        }) {
            bail!("maintainer {login:?} is not a GitHub login");
        }
        if config.start_prompt.trim().is_empty() {
            bail!("start_prompt must not be empty");
        }
        if config.refresh_interval_secs < MINIMUM_REFRESH {
            bail!("refresh_interval_secs must be at least {MINIMUM_REFRESH}");
        }
        Ok(config)
    }

    pub fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.refresh_interval_secs)
    }

    pub fn clone_command(&self) -> String {
        let name = self.repository.rsplit('/').next().unwrap_or_default();
        format!(
            "git clone https://github.com/{} && cd {name}",
            self.repository
        )
    }

    pub fn is_maintainer(&self, login: &str) -> bool {
        self.maintainers
            .iter()
            .any(|maintainer| maintainer.eq_ignore_ascii_case(login))
    }
}

#[cfg(test)]
pub fn test_config() -> Config {
    Config::parse(
        r#"
        repository = "LyraCoreProject/LyraCore"
        maintainers = ["Mathih13"]
        listen = "127.0.0.1:8080"
        start_prompt = "Follow the skill and fix one issue."
        "#,
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_configuration_parses() {
        let config = Config::parse(include_str!("../workshop.example.toml")).unwrap();
        assert_eq!(config.repository, "LyraCoreProject/LyraCore");
        assert!(config.is_maintainer("mathih13"));
        assert_eq!(config.refresh_interval_secs, 900);
    }

    #[test]
    fn a_repository_that_could_change_the_api_path_is_refused() {
        let text = "repository = \"a/b/../c\"\nlisten = \"127.0.0.1:1\"";
        assert!(Config::parse(text).is_err());
    }
}
