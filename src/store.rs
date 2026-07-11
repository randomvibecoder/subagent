use crate::{
    config::{Paths, ensure_private_dir, write_private_atomic},
    ipc::{AgentMode, ListFilter, coded_error},
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
    pub name: String,
    pub dir: String,
    pub mode: AgentMode,
    pub advisory_readonly: bool,
    pub model: String,
    pub status: AgentStatus,
    pub spawned_at: DateTime<Utc>,
    pub last_message_at: DateTime<Utc>,
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

#[derive(Debug, Clone, Serialize)]
pub struct AgentListItem {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    pub status: AgentStatus,
    pub dir: String,
    pub mode: AgentMode,
    pub spawned_at: DateTime<Utc>,
    pub last_message_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub run_number: u64,
    pub working_sides: usize,
}

impl AgentListItem {
    pub fn from_metadata(meta: AgentMetadata, working_sides: usize) -> Self {
        Self {
            kind: "agent_list_item",
            id: meta.id,
            name: meta.name,
            status: meta.status,
            dir: meta.dir,
            mode: meta.mode,
            spawned_at: meta.spawned_at,
            last_message_at: meta.last_message_at,
            updated_at: meta.updated_at,
            run_number: meta.run_number,
            working_sides,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideMetadata {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub agent_id: String,
    pub status: AgentStatus,
    pub question: String,
    pub answer: Option<String>,
    pub model: String,
    pub mode: AgentMode,
    pub parent_mode: AgentMode,
    pub created_at: DateTime<Utc>,
    pub run_started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub deadline_at: Option<DateTime<Utc>>,
    pub inherited_context_messages: usize,
    pub tool_calls: usize,
    pub stop_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SideListItem {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    pub agent_id: String,
    pub status: AgentStatus,
    pub question_preview: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tool_calls: usize,
}

impl From<SideMetadata> for SideListItem {
    fn from(meta: SideMetadata) -> Self {
        Self {
            kind: "side_list_item",
            id: meta.id,
            agent_id: meta.agent_id,
            status: meta.status,
            question_preview: meta.question.chars().take(200).collect(),
            created_at: meta.created_at,
            updated_at: meta.updated_at,
            tool_calls: meta.tool_calls,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Pending,
    Delivered,
    Cancelled,
}

impl MessageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub agent_id: String,
    pub content: String,
    pub status: MessageStatus,
    pub sent_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub event_id: String,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub side_id: Option<String>,
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
    #[serde(default)]
    pub delivered_message_ids: Vec<String>,
}

#[derive(Clone)]
pub struct Store {
    agents_root: PathBuf,
    sides_root: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl Store {
    pub fn new(paths: &Paths) -> Result<Self> {
        ensure_private_dir(&paths.state_dir)?;
        ensure_private_dir(&paths.agents_dir())?;
        ensure_private_dir(&paths.sides_dir())?;
        Ok(Self {
            agents_root: paths.agents_dir(),
            sides_root: paths.sides_dir(),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn agent_dir(&self, id: &str) -> PathBuf {
        self.agents_root.join(id)
    }
    pub fn side_dir(&self, id: &str) -> PathBuf {
        self.sides_root.join(id)
    }
    fn owner_dir(&self, id: &str) -> PathBuf {
        if id.starts_with("side_") {
            self.side_dir(id)
        } else {
            self.agent_dir(id)
        }
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
    fn messages_path(&self, id: &str) -> PathBuf {
        self.agent_dir(id).join("messages.json")
    }
    pub fn outputs_dir(&self, id: &str) -> PathBuf {
        self.owner_dir(id).join("outputs")
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
        self.save_messages(&meta.id, &[])?;
        self.append_event(
            &meta.id,
            "lifecycle",
            json!({"status":"working","reason":"spawned"}),
        )?;
        Ok(())
    }

    pub fn load_metadata(&self, id: &str) -> Result<AgentMetadata> {
        let path = self.metadata_path(id);
        let body = fs::read(&path).map_err(|_| {
            coded_error(
                "agent_not_found",
                format!("agent not found: {id}"),
                json!({"agent_id":id}),
                false,
            )
        })?;
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

    pub fn read_messages(&self, id: &str) -> Result<Vec<MessageRecord>> {
        self.load_metadata(id)?;
        let path = self.messages_path(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("parse {}", path.display()))
    }

    fn save_messages(&self, id: &str, messages: &[MessageRecord]) -> Result<()> {
        write_private_atomic(
            &self.messages_path(id),
            &serde_json::to_vec_pretty(messages)?,
        )
    }

    pub fn enqueue_message(&self, id: &str, content: String) -> Result<MessageRecord> {
        let _guard = self.write_lock.lock().unwrap();
        let mut messages = self.read_messages(id)?;
        let message = MessageRecord {
            kind: "message".into(),
            id: format!("msg_{}", ulid::Ulid::new()),
            agent_id: id.into(),
            content,
            status: MessageStatus::Pending,
            sent_at: Utc::now(),
            delivered_at: None,
            cancelled_at: None,
        };
        messages.push(message.clone());
        self.save_messages(id, &messages)?;
        let mut meta = self.load_metadata(id)?;
        meta.last_message_at = message.sent_at;
        self.save_metadata(&meta)?;
        Ok(message)
    }

    pub fn pending_messages(&self, id: &str) -> Result<Vec<MessageRecord>> {
        Ok(self
            .read_messages(id)?
            .into_iter()
            .filter(|message| message.status == MessageStatus::Pending)
            .collect())
    }

    pub fn load_message(&self, id: &str, message_id: &str) -> Result<MessageRecord> {
        self.read_messages(id)?
            .into_iter()
            .find(|message| message.id == message_id)
            .ok_or_else(|| {
                coded_error(
                    "message_not_found",
                    format!("message not found: {message_id}"),
                    json!({"agent_id":id,"message_id":message_id}),
                    false,
                )
            })
    }

    pub fn mark_message_delivered(&self, id: &str, message_id: &str) -> Result<MessageRecord> {
        self.update_message(id, message_id, MessageStatus::Delivered)
    }

    pub fn cancel_message(&self, id: &str, message_id: &str) -> Result<MessageRecord> {
        self.update_message(id, message_id, MessageStatus::Cancelled)
    }

    pub fn cancel_pending_messages(&self, id: &str) -> Result<()> {
        let ids = self
            .pending_messages(id)?
            .into_iter()
            .map(|message| message.id)
            .collect::<Vec<_>>();
        for message_id in ids {
            self.cancel_message(id, &message_id)?;
        }
        Ok(())
    }

    fn update_message(
        &self,
        id: &str,
        message_id: &str,
        status: MessageStatus,
    ) -> Result<MessageRecord> {
        let _guard = self.write_lock.lock().unwrap();
        let mut messages = self.read_messages(id)?;
        let message = messages
            .iter_mut()
            .find(|message| message.id == message_id)
            .ok_or_else(|| {
                coded_error(
                    "message_not_found",
                    format!("message not found: {message_id}"),
                    json!({"agent_id":id,"message_id":message_id}),
                    false,
                )
            })?;
        if message.status != MessageStatus::Pending {
            return Err(coded_error(
                "conflict",
                format!("message is not pending: {message_id}"),
                json!({"agent_id":id,"message_id":message_id,"status":message.status.as_str()}),
                false,
            ));
        }
        let now = Utc::now();
        message.status = status;
        match message.status {
            MessageStatus::Delivered => message.delivered_at = Some(now),
            MessageStatus::Cancelled => message.cancelled_at = Some(now),
            MessageStatus::Pending => {}
        }
        let result = message.clone();
        self.save_messages(id, &messages)?;
        Ok(result)
    }

    pub fn has_message_event(&self, id: &str, message_id: &str) -> Result<bool> {
        Ok(self.read_events(id)?.iter().any(|event| {
            event.event_type == "user_message"
                && event.data.get("message_id").and_then(Value::as_str) == Some(message_id)
        }))
    }

    pub fn append_event(&self, id: &str, event_type: &str, data: Value) -> Result<EventRecord> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let _guard = self.write_lock.lock().unwrap();
        let sequence = self.event_count(id)? + 1;
        let event = EventRecord {
            event_id: format!("evt_{}", ulid::Ulid::new()),
            agent_id: id.to_string(),
            side_id: None,
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
        let mut meta = self.load_metadata(id)?;
        meta.updated_at = event.timestamp;
        self.save_metadata(&meta)?;
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
        for entry in fs::read_dir(&self.agents_root)? {
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

    pub fn create_side(&self, meta: &SideMetadata, context: &ContextSnapshot) -> Result<()> {
        let dir = self.side_dir(&meta.id);
        if dir.exists() {
            bail!("side already exists: {}", meta.id);
        }
        ensure_private_dir(&dir)?;
        ensure_private_dir(&self.outputs_dir(&meta.id))?;
        self.save_side_metadata(meta)?;
        write_private_atomic(
            &dir.join("context.json"),
            &serde_json::to_vec_pretty(context)?,
        )?;
        self.append_side_event(
            &meta.id,
            "lifecycle",
            json!({"status":"working","reason":"created"}),
        )?;
        Ok(())
    }

    pub fn load_side_metadata(&self, id: &str) -> Result<SideMetadata> {
        let path = self.side_dir(id).join("metadata.json");
        let body = fs::read(&path).map_err(|_| {
            coded_error(
                "side_not_found",
                format!("side not found: {id}"),
                json!({"side_id":id}),
                false,
            )
        })?;
        serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save_side_metadata(&self, meta: &SideMetadata) -> Result<()> {
        write_private_atomic(
            &self.side_dir(&meta.id).join("metadata.json"),
            &serde_json::to_vec_pretty(meta)?,
        )
    }

    pub fn save_side_context(&self, id: &str, context: &ContextSnapshot) -> Result<()> {
        write_private_atomic(
            &self.side_dir(id).join("context.json"),
            &serde_json::to_vec_pretty(context)?,
        )
    }

    pub fn append_side_event(
        &self,
        id: &str,
        event_type: &str,
        data: Value,
    ) -> Result<EventRecord> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let _guard = self.write_lock.lock().unwrap();
        let mut meta = self.load_side_metadata(id)?;
        let path = self.side_dir(id).join("events.jsonl");
        let sequence = if path.exists() {
            BufReader::new(fs::File::open(&path)?).lines().count() as u64 + 1
        } else {
            1
        };
        let event = EventRecord {
            event_id: format!("evt_{}", ulid::Ulid::new()),
            agent_id: meta.agent_id.clone(),
            side_id: Some(id.to_string()),
            sequence,
            timestamp: Utc::now(),
            event_type: event_type.to_string(),
            data,
        };
        let mut opts = fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut file = opts.open(path)?;
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        meta.updated_at = event.timestamp;
        self.save_side_metadata(&meta)?;
        Ok(event)
    }

    pub fn read_side_events(&self, id: &str) -> Result<Vec<EventRecord>> {
        self.load_side_metadata(id)?;
        let path = self.side_dir(id).join("events.jsonl");
        let file = fs::File::open(&path).with_context(|| format!("side not found: {id}"))?;
        BufReader::new(file)
            .lines()
            .filter_map(|line| match line {
                Ok(value) if !value.trim().is_empty() => {
                    Some(serde_json::from_str(&value).map_err(anyhow::Error::from))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error.into())),
            })
            .collect()
    }

    pub fn list_sides(&self, agent_id: &str) -> Result<Vec<SideMetadata>> {
        self.load_metadata(agent_id)?;
        let mut sides = Vec::new();
        for entry in fs::read_dir(&self.sides_root)? {
            let path = entry?.path().join("metadata.json");
            if !path.exists() {
                continue;
            }
            let Ok(meta) = serde_json::from_slice::<SideMetadata>(&fs::read(path)?) else {
                continue;
            };
            if meta.agent_id == agent_id {
                sides.push(meta);
            }
        }
        sides.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(sides)
    }

    pub fn working_side_count(&self, agent_id: &str) -> Result<usize> {
        Ok(self
            .list_sides(agent_id)?
            .into_iter()
            .filter(|side| side.status == AgentStatus::Working)
            .count())
    }

    pub fn recover_interrupted_sides(&self) -> Result<usize> {
        let mut count = 0;
        for entry in fs::read_dir(&self.sides_root)? {
            let id = entry?.file_name().to_string_lossy().into_owned();
            let Ok(mut meta) = self.load_side_metadata(&id) else {
                continue;
            };
            if meta.status == AgentStatus::Working {
                let now = Utc::now();
                meta.status = AgentStatus::Stopped;
                meta.updated_at = now;
                meta.stopped_at = Some(now);
                meta.deadline_at = None;
                meta.stop_reason = Some("daemon_interrupted".into());
                self.save_side_metadata(&meta)?;
                self.append_side_event(
                    &id,
                    "lifecycle",
                    json!({"status":"stopped","reason":"daemon_interrupted"}),
                )?;
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn delete_side(&self, id: &str) -> Result<()> {
        let meta = self.load_side_metadata(id)?;
        if meta.status == AgentStatus::Working {
            return Err(coded_error(
                "conflict",
                "cannot delete a working side",
                json!({"side_id":id,"status":"working"}),
                false,
            ));
        }
        fs::remove_dir_all(self.side_dir(id))?;
        Ok(())
    }

    pub fn delete_sides_for_agent(&self, agent_id: &str) -> Result<()> {
        for side in self.list_sides(agent_id)? {
            if self.side_dir(&side.id).exists() {
                fs::remove_dir_all(self.side_dir(&side.id))?;
            }
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let meta = self.load_metadata(id)?;
        if meta.status == AgentStatus::Working {
            return Err(coded_error(
                "conflict",
                "cannot delete a working agent",
                json!({"agent_id":id,"status":"working"}),
                false,
            ));
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
    if let Some(dir) = &f.dir
        && &m.dir != dir
    {
        return Ok(false);
    }
    if let Some(t) = parse_time(&f.spawned_after)?
        && m.spawned_at < t
    {
        return Ok(false);
    }
    if let Some(t) = parse_time(&f.spawned_before)?
        && m.spawned_at > t
    {
        return Ok(false);
    }
    if let Some(t) = parse_time(&f.finished_after)?
        && m.finished_at.is_none_or(|x| x < t)
    {
        return Ok(false);
    }
    if let Some(t) = parse_time(&f.finished_before)?
        && m.finished_at.is_none_or(|x| x > t)
    {
        return Ok(false);
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

pub fn normalize_agent_name(name: &str) -> Result<String> {
    let name = name.trim();
    let length = name.chars().count();
    if !(4..=40).contains(&length) {
        bail!("agent name must contain 4 through 40 characters");
    }
    if name.chars().any(char::is_control) {
        bail!("agent name must not contain control characters");
    }
    Ok(name.to_string())
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

pub fn canonical_filter_dir(dir: &str) -> Result<String> {
    let path = Path::new(dir);
    if path.exists() {
        return canonical_dir(dir);
    }
    if !path.is_absolute() {
        bail!("a non-existing --dir filter must be an absolute stored path");
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::Prefix(_) => unreachable!("Windows paths are unsupported"),
        }
    }
    Ok(normalized.to_string_lossy().into_owned())
}

#[cfg(test)]
mod name_tests {
    use super::*;
    use crate::config::Paths;

    #[test]
    fn agent_names_are_trimmed_and_bounded() {
        assert_eq!(
            normalize_agent_name("  Build site  ").unwrap(),
            "Build site"
        );
        assert!(normalize_agent_name("abc").is_err());
        assert!(normalize_agent_name(&"x".repeat(41)).is_err());
        assert!(normalize_agent_name("bad\nname").is_err());
    }

    #[test]
    fn side_history_persists_and_recovers_as_stopped() {
        let root = std::env::temp_dir().join(format!("subagent-side-store-{}", ulid::Ulid::new()));
        let paths = Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        };
        let store = Store::new(&paths).unwrap();
        let now = Utc::now();
        let agent = AgentMetadata {
            kind: "agent".into(),
            id: "agt_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            name: "Parent".into(),
            dir: root.to_string_lossy().into_owned(),
            mode: AgentMode::Write,
            advisory_readonly: false,
            model: "test".into(),
            status: AgentStatus::Finished,
            spawned_at: now,
            last_message_at: now,
            run_started_at: now,
            updated_at: now,
            finished_at: Some(now),
            stopped_at: None,
            failed_at: None,
            deadline_at: None,
            run_number: 1,
            stop_reason: None,
            last_error: None,
        };
        store.create(&agent, &ContextSnapshot::default()).unwrap();
        let side = SideMetadata {
            kind: "side".into(),
            id: "side_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            agent_id: agent.id.clone(),
            status: AgentStatus::Working,
            question: "Inspect this".into(),
            answer: None,
            model: "test".into(),
            mode: AgentMode::Readonly,
            parent_mode: AgentMode::Write,
            created_at: now,
            run_started_at: now,
            updated_at: now,
            finished_at: None,
            stopped_at: None,
            failed_at: None,
            deadline_at: None,
            inherited_context_messages: 2,
            tool_calls: 0,
            stop_reason: None,
            last_error: None,
        };
        store
            .create_side(&side, &ContextSnapshot::default())
            .unwrap();
        let event = store
            .append_side_event(
                &side.id,
                "user_message",
                json!({"content":"Inspect this","source":"create"}),
            )
            .unwrap();
        assert_eq!(event.side_id.as_deref(), Some(side.id.as_str()));
        assert_eq!(store.working_side_count(&agent.id).unwrap(), 1);
        assert_eq!(store.recover_interrupted_sides().unwrap(), 1);
        assert_eq!(
            store.load_side_metadata(&side.id).unwrap().status,
            AgentStatus::Stopped
        );
        store.delete_sides_for_agent(&agent.id).unwrap();
        assert!(!store.side_dir(&side.id).exists());
        let _ = fs::remove_dir_all(root);
    }
}
