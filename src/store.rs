use crate::{
    config::{Paths, ensure_private_dir, write_private_atomic},
    ipc::{AgentMode, ListFilter},
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Working,
    Finished,
    Stopped,
    Failed,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Finished => "finished",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetadata {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub title: String,
    pub dir: String,
    pub mode: AgentMode,
    pub advisory_readonly: bool,
    pub model: String,
    pub status: AgentStatus,
    pub spawned_at: DateTime<Utc>,
    pub run_started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub deadline_at: Option<DateTime<Utc>>,
    pub run_number: u64,
    pub stop_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub event_id: String,
    pub agent_id: String,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextSnapshot {
    pub messages: Vec<Value>,
    pub compacted_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl Store {
    pub fn new(paths: &Paths) -> Result<Self> {
        ensure_private_dir(&paths.state_dir)?;
        ensure_private_dir(&paths.agents_dir())?;
        Ok(Self {
            root: paths.agents_dir(),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn agent_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
    fn metadata_path(&self, id: &str) -> PathBuf {
        self.agent_dir(id).join("metadata.json")
    }
    fn events_path(&self, id: &str) -> PathBuf {
        self.agent_dir(id).join("events.jsonl")
    }
    fn context_path(&self, id: &str) -> PathBuf {
        self.agent_dir(id).join("context.json")
    }
    pub fn outputs_dir(&self, id: &str) -> PathBuf {
        self.agent_dir(id).join("outputs")
    }

    pub fn create(&self, meta: &AgentMetadata, context: &ContextSnapshot) -> Result<()> {
        let dir = self.agent_dir(&meta.id);
        if dir.exists() {
            bail!("agent already exists: {}", meta.id);
        }
        ensure_private_dir(&dir)?;
        ensure_private_dir(&self.outputs_dir(&meta.id))?;
        self.save_metadata(meta)?;
        self.save_context(&meta.id, context)?;
        self.append_event(
            &meta.id,
            "lifecycle",
            json!({"status":"working","reason":"spawned"}),
        )?;
        Ok(())
    }

    pub fn load_metadata(&self, id: &str) -> Result<AgentMetadata> {
        let path = self.metadata_path(id);
        let body = fs::read(&path).with_context(|| format!("agent not found: {id}"))?;
        serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save_metadata(&self, meta: &AgentMetadata) -> Result<()> {
        write_private_atomic(
            &self.metadata_path(&meta.id),
            &serde_json::to_vec_pretty(meta)?,
        )
    }

    pub fn load_context(&self, id: &str) -> Result<ContextSnapshot> {
        let path = self.context_path(id);
        serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("parse {}", path.display()))
    }

    pub fn save_context(&self, id: &str, context: &ContextSnapshot) -> Result<()> {
        write_private_atomic(&self.context_path(id), &serde_json::to_vec_pretty(context)?)
    }

    pub fn append_event(&self, id: &str, event_type: &str, data: Value) -> Result<EventRecord> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let _guard = self.write_lock.lock().unwrap();
        let sequence = self.event_count(id)? + 1;
        let event = EventRecord {
            event_id: format!("evt_{}", ulid::Ulid::new()),
            agent_id: id.to_string(),
            sequence,
            timestamp: Utc::now(),
            event_type: event_type.to_string(),
            data,
        };
        let path = self.events_path(id);
        let mut opts = fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut file = opts.open(path)?;
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(event)
    }

    fn event_count(&self, id: &str) -> Result<u64> {
        let path = self.events_path(id);
        if !path.exists() {
            return Ok(0);
        }
        Ok(BufReader::new(fs::File::open(path)?).lines().count() as u64)
    }

    pub fn read_events(&self, id: &str) -> Result<Vec<EventRecord>> {
        let path = self.events_path(id);
        let file = fs::File::open(&path).with_context(|| format!("agent not found: {id}"))?;
        BufReader::new(file)
            .lines()
            .filter_map(|line| match line {
                Ok(s) if !s.trim().is_empty() => {
                    Some(serde_json::from_str(&s).map_err(anyhow::Error::from))
                }
                Ok(_) => None,
                Err(e) => Some(Err(e.into())),
            })
            .collect()
    }

    pub fn list(&self, filter: &ListFilter) -> Result<Vec<AgentMetadata>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path().join("metadata.json");
            if !path.exists() {
                continue;
            }
            let Ok(meta) = serde_json::from_slice::<AgentMetadata>(&fs::read(path)?) else {
                continue;
            };
            if !matches_filter(&meta, filter)? {
                continue;
            }
            out.push(meta);
        }
        out.sort_by(|a, b| compare_meta(a, b, &filter.sort));
        if filter.order != "asc" {
            out.reverse();
        }
        Ok(out
            .into_iter()
            .skip(filter.offset)
            .take(filter.limit)
            .collect())
    }

    pub fn recover_interrupted(&self) -> Result<usize> {
        let filter = ListFilter {
            limit: usize::MAX,
            sort: "spawned_at".into(),
            order: "asc".into(),
            ..Default::default()
        };
        let mut count = 0;
        for mut meta in self.list(&filter)? {
            if meta.status == AgentStatus::Working {
                let now = Utc::now();
                meta.status = AgentStatus::Stopped;
                meta.updated_at = now;
                meta.stopped_at = Some(now);
                meta.stop_reason = Some("daemon_interrupted".into());
                self.save_metadata(&meta)?;
                self.append_event(
                    &meta.id,
                    "lifecycle",
                    json!({"status":"stopped","reason":"daemon_interrupted"}),
                )?;
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let meta = self.load_metadata(id)?;
        if meta.status == AgentStatus::Working {
            bail!("cannot delete a working agent");
        }
        fs::remove_dir_all(self.agent_dir(id))?;
        Ok(())
    }

    pub fn output_path(&self, id: &str, output_ref: &str) -> Result<PathBuf> {
        if !output_ref.starts_with("out_") || output_ref.contains('/') || output_ref.contains("..")
        {
            bail!("invalid output reference");
        }
        Ok(self.outputs_dir(id).join(format!("{output_ref}.log")))
    }
}

fn parse_time(v: &Option<String>) -> Result<Option<DateTime<Utc>>> {
    v.as_ref()
        .map(|s| {
            DateTime::parse_from_rfc3339(s)
                .map(|v| v.with_timezone(&Utc))
                .map_err(anyhow::Error::from)
        })
        .transpose()
}

fn matches_filter(m: &AgentMetadata, f: &ListFilter) -> Result<bool> {
    if !f.statuses.is_empty() && !f.statuses.iter().any(|s| s == m.status.as_str()) {
        return Ok(false);
    }
    if let Some(dir) = &f.dir {
        if &m.dir != dir {
            return Ok(false);
        }
    }
    if let Some(t) = parse_time(&f.spawned_after)? {
        if m.spawned_at < t {
            return Ok(false);
        }
    }
    if let Some(t) = parse_time(&f.spawned_before)? {
        if m.spawned_at > t {
            return Ok(false);
        }
    }
    if let Some(t) = parse_time(&f.finished_after)? {
        if m.finished_at.is_none_or(|x| x < t) {
            return Ok(false);
        }
    }
    if let Some(t) = parse_time(&f.finished_before)? {
        if m.finished_at.is_none_or(|x| x > t) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn compare_meta(a: &AgentMetadata, b: &AgentMetadata, key: &str) -> Ordering {
    match key {
        "updated_at" => a.updated_at.cmp(&b.updated_at),
        "finished_at" => a.finished_at.cmp(&b.finished_at),
        _ => a.spawned_at.cmp(&b.spawned_at),
    }
    .then_with(|| a.id.cmp(&b.id))
}

pub fn title_from_message(message: &str) -> String {
    let first = message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Untitled agent")
        .trim();
    first.chars().take(80).collect()
}

pub fn canonical_dir(dir: &str) -> Result<String> {
    let path = Path::new(dir)
        .canonicalize()
        .with_context(|| format!("invalid directory: {dir}"))?;
    if !path.is_dir() {
        bail!("not a directory: {dir}");
    }
    Ok(path.to_string_lossy().into_owned())
}
