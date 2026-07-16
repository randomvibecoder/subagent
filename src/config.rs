use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, env, fs, path::PathBuf};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-5.4-mini";
pub const CONFIG_KEYS: [&str; 6] = [
    "base-url",
    "model",
    "max-agents",
    "context-token-budget",
    "tool-output-preview-bytes",
    "stall-notification-seconds",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub base_url: String,
    pub model: String,
    pub max_agents: usize,
    pub context_token_budget: usize,
    pub tool_output_preview_bytes: usize,
    pub stall_notification_seconds: u64,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            max_agents: 8,
            context_token_budget: 64_000,
            tool_output_preview_bytes: 16 * 1024,
            stall_notification_seconds: 180,
        }
    }
}

#[derive(Clone)]
pub struct RuntimeConfig {
    pub file: FileConfig,
    pub sources: BTreeMap<String, String>,
    pub api_key: String,
    pub web_password: Option<String>,
    pub paths: Paths,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let config_dir = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("subagent");
        let state_dir = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("subagent");
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("run"));
        Ok(Self {
            config_dir,
            state_dir,
            runtime_dir,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("subagent.sock")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }
    pub fn daemon_lock(&self) -> PathBuf {
        self.runtime_dir.join("subagent.lock")
    }
    pub fn daemon_state(&self) -> PathBuf {
        self.state_dir.join("daemon-state.json")
    }
    pub fn agents_dir(&self) -> PathBuf {
        self.state_dir.join("agents")
    }
    pub fn sides_dir(&self) -> PathBuf {
        self.state_dir.join("sides")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonLifecycle {
    pub status: String,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: String,
}

pub fn read_daemon_lifecycle(paths: &Paths) -> Result<Option<DaemonLifecycle>> {
    let path = paths.daemon_state();
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

pub fn write_daemon_lifecycle(
    paths: &Paths,
    status: &str,
    pid: u32,
    started_at: DateTime<Utc>,
) -> Result<()> {
    ensure_private_dir(&paths.state_dir)?;
    let state = DaemonLifecycle {
        status: status.into(),
        pid,
        started_at,
        updated_at: Utc::now(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let path = paths.daemon_state();
    let temporary = path.with_extension("json.tmp");
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    use std::io::Write;
    let mut file = options.open(&temporary)?;
    serde_json::to_writer(&mut file, &state)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

impl FileConfig {
    pub fn load_persisted(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        let cfg = if path.exists() {
            toml::from_str(
                &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
            )
            .with_context(|| format!("parse {}", path.display()))?
        } else {
            Self::default()
        };
        Ok(cfg)
    }

    pub fn load_with_sources(paths: &Paths) -> Result<(Self, BTreeMap<String, String>)> {
        let mut cfg = Self::load_persisted(paths)?;
        let persisted = persisted_key_values(paths)?;
        let mut sources = CONFIG_KEYS
            .iter()
            .map(|key| {
                (
                    (*key).to_string(),
                    if persisted.contains_key(*key) {
                        "persisted"
                    } else {
                        "default"
                    }
                    .to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if let Ok(v) = env::var("OPENAI_BASE_URL") {
            cfg.base_url = v;
            sources.insert("base-url".into(), "OPENAI_BASE_URL".into());
        }
        if let Ok(v) = env::var("OPENAI_MODEL") {
            cfg.model = v;
            sources.insert("model".into(), "OPENAI_MODEL".into());
        }
        if let Ok(v) = env::var("SUBAGENT_MAX_AGENTS") {
            cfg.max_agents = v
                .parse()
                .context("SUBAGENT_MAX_AGENTS must be a non-negative integer")?;
            sources.insert("max-agents".into(), "SUBAGENT_MAX_AGENTS".into());
        }
        if let Ok(v) = env::var("SUBAGENT_STALL_NOTIFICATION_SECONDS") {
            cfg.stall_notification_seconds = v.parse().context(
                "SUBAGENT_STALL_NOTIFICATION_SECONDS must be an integer from 0 through 86400",
            )?;
            sources.insert(
                "stall-notification-seconds".into(),
                "SUBAGENT_STALL_NOTIFICATION_SECONDS".into(),
            );
        }
        cfg.validate()?;
        Ok((cfg, sources))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        ensure_private_dir(&paths.config_dir)?;
        let body = toml::to_string_pretty(self)?;
        write_private_atomic(&paths.config_file(), body.as_bytes())
    }

    pub fn get(&self, key: &str) -> Result<serde_json::Value> {
        Ok(match key {
            "base-url" => self.base_url.clone().into(),
            "model" => self.model.clone().into(),
            "max-agents" => self.max_agents.into(),
            "context-token-budget" => self.context_token_budget.into(),
            "tool-output-preview-bytes" => self.tool_output_preview_bytes.into(),
            "stall-notification-seconds" => self.stall_notification_seconds.into(),
            _ => bail!("unknown config key: {key}"),
        })
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "base-url" => {
                if value.trim().is_empty() {
                    bail!("base-url must not be empty")
                }
                self.base_url = value.to_string()
            }
            "model" => {
                if value.trim().is_empty() {
                    bail!("model must not be empty")
                }
                self.model = value.to_string()
            }
            "max-agents" => {
                self.max_agents = value
                    .parse()
                    .context("max-agents must be a non-negative integer")?
            }
            "context-token-budget" => {
                self.context_token_budget = value
                    .parse()
                    .context("context-token-budget must be a positive integer")?;
                if self.context_token_budget == 0 {
                    bail!("context-token-budget must be a positive integer")
                }
            }
            "tool-output-preview-bytes" => {
                self.tool_output_preview_bytes = value
                    .parse()
                    .context("tool-output-preview-bytes must be a positive integer")?;
                if self.tool_output_preview_bytes == 0 {
                    bail!("tool-output-preview-bytes must be a positive integer")
                }
            }
            "stall-notification-seconds" => {
                self.stall_notification_seconds = value.parse().context(
                    "stall-notification-seconds must be an integer from 0 through 86400",
                )?;
                if self.stall_notification_seconds > 86_400 {
                    bail!("stall-notification-seconds must be from 0 through 86400")
                }
            }
            _ => bail!("unknown config key: {key}"),
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            bail!("base-url must not be empty")
        }
        if self.model.trim().is_empty() {
            bail!("model must not be empty")
        }
        if self.context_token_budget == 0 {
            bail!("context-token-budget must be a positive integer")
        }
        if self.tool_output_preview_bytes == 0 {
            bail!("tool-output-preview-bytes must be a positive integer")
        }
        if self.stall_notification_seconds > 86_400 {
            bail!("stall-notification-seconds must be from 0 through 86400")
        }
        Ok(())
    }
}

impl RuntimeConfig {
    pub fn load() -> Result<Self> {
        let paths = Paths::discover()?;
        let (file, sources) = FileConfig::load_with_sources(&paths)?;
        let api_key =
            env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is required to start the daemon")?;
        if api_key.trim().is_empty() {
            bail!("OPENAI_API_KEY is empty");
        }
        let web_password = match env::var("SUBAGENT_WEB_PASSWORD") {
            Ok(value) if value.is_empty() => bail!("SUBAGENT_WEB_PASSWORD is empty"),
            Ok(value) => Some(value),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => bail!("SUBAGENT_WEB_PASSWORD is not UTF-8"),
        };
        Ok(Self {
            file,
            sources,
            api_key,
            web_password,
            paths,
        })
    }
}

pub fn persisted_key_values(paths: &Paths) -> Result<BTreeMap<String, Value>> {
    let path = paths.config_file();
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let value: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
    let Some(table) = value.as_table() else {
        return Ok(BTreeMap::new());
    };
    let mut values = BTreeMap::new();
    for key in CONFIG_KEYS {
        let stored = key.replace('-', "_");
        if let Some(value) = table.get(&stored) {
            values.insert(key.into(), serde_json::to_value(value)?);
        }
    }
    Ok(values)
}

pub fn local_config_values(paths: &Paths) -> Result<Vec<Value>> {
    let defaults = FileConfig::default();
    let persisted = persisted_key_values(paths)?;
    let (effective, sources) = FileConfig::load_with_sources(paths)?;
    CONFIG_KEYS
        .iter()
        .map(|key| {
            Ok(serde_json::json!({
                "type":"config_value",
                "key":key,
                "default_value":defaults.get(key)?,
                "persisted_value":persisted.get(*key).cloned(),
                "local_effective_value":effective.get(key)?,
                "local_source":sources.get(*key),
                "active_value":Value::Null,
                "active_source":Value::Null,
                "restart_required":Value::Null,
            }))
        })
        .collect()
}

#[cfg(unix)]
pub fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn write_private_atomic(path: &std::path::Path, body: &[u8]) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let mut opts = fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(&tmp)?;
    file.write_all(body)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_config_rejects_empty_and_zero_values() {
        let mut config = FileConfig::default();
        assert_eq!(config.max_agents, 8);
        assert!(config.set("base-url", "").is_err());
        assert!(config.set("model", " ").is_err());
        assert!(config.set("context-token-budget", "0").is_err());
        assert!(config.set("tool-output-preview-bytes", "0").is_err());
        assert!(config.set("max-agents", "0").is_ok());
        assert!(config.set("stall-notification-seconds", "0").is_ok());
        assert!(config.set("stall-notification-seconds", "86401").is_err());
    }
}
