use crate::{
    config::{Paths, ensure_private_dir, write_private_atomic},
    ipc::{AgentMode, ListFilter, coded_error},
};
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    collections::VecDeque,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    #[default]
    Starting,
    ProcessingMessages,
    RequestingModel,
    RetryingModel,
    ExecutingTool,
    WaitingTerminal,
    Finished,
    Stopped,
    Failed,
}

impl AgentPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::ProcessingMessages => "processing_messages",
            Self::RequestingModel => "requesting_model",
            Self::RetryingModel => "retrying_model",
            Self::ExecutingTool => "executing_tool",
            Self::WaitingTerminal => "waiting_terminal",
            Self::Finished => "finished",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActivityTelemetry {
    #[serde(default)]
    pub current_phase: AgentPhase,
    #[serde(default)]
    pub phase_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub request_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_provider_activity_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub provider_request_id: Option<String>,
    #[serde(default)]
    pub last_provider_request_id: Option<String>,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub last_event_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_model_event_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_tool_event_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_state_change_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_progress_at: Option<DateTime<Utc>>,
}

impl ActivityTelemetry {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            current_phase: AgentPhase::Starting,
            phase_started_at: Some(now),
            last_state_change_at: Some(now),
            last_progress_at: Some(now),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetadata {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(rename = "ref", default)]
    pub local_ref: String,
    pub name: String,
    pub dir: String,
    pub mode: AgentMode,
    pub advisory_readonly: bool,
    pub model: String,
    pub status: AgentStatus,
    pub spawned_at: DateTime<Utc>,
    pub last_message_at: DateTime<Utc>,
    #[serde(default)]
    pub last_message_sent_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_message_delivered_at: Option<DateTime<Utc>>,
    pub run_started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub deadline_at: Option<DateTime<Utc>>,
    pub run_number: u64,
    pub stop_reason: Option<String>,
    pub last_error: Option<String>,
    #[serde(flatten)]
    pub activity: ActivityTelemetry,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentListItem {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    #[serde(rename = "ref")]
    pub local_ref: String,
    pub name: String,
    pub status: AgentStatus,
    pub dir: String,
    pub mode: AgentMode,
    pub model: String,
    pub spawned_at: DateTime<Utc>,
    pub last_message_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub current_phase: AgentPhase,
    pub last_event_at: Option<DateTime<Utc>>,
    pub run_number: u64,
    pub working_sides: usize,
}

impl AgentListItem {
    pub fn from_metadata(meta: AgentMetadata, working_sides: usize) -> Self {
        Self {
            kind: "agent_list_item",
            id: meta.id,
            local_ref: meta.local_ref,
            name: meta.name,
            status: meta.status,
            dir: meta.dir,
            mode: meta.mode,
            model: meta.model,
            spawned_at: meta.spawned_at,
            last_message_at: meta.last_message_at,
            updated_at: meta.updated_at,
            current_phase: meta.activity.current_phase,
            last_event_at: meta.activity.last_event_at,
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
    #[serde(rename = "ref", default)]
    pub local_ref: String,
    pub agent_id: String,
    #[serde(default)]
    pub agent_ref: String,
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
    #[serde(flatten)]
    pub activity: ActivityTelemetry,
}

#[derive(Debug, Clone, Serialize)]
pub struct SideListItem {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
    #[serde(rename = "ref")]
    pub local_ref: String,
    pub agent_id: String,
    pub agent_ref: String,
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
            local_ref: meta.local_ref,
            agent_id: meta.agent_id,
            agent_ref: meta.agent_ref,
            status: meta.status,
            question_preview: meta.question.chars().take(200).collect(),
            created_at: meta.created_at,
            updated_at: meta.updated_at,
            tool_calls: meta.tool_calls,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
    #[serde(rename = "ref", default)]
    pub local_ref: String,
    pub agent_id: String,
    #[serde(default)]
    pub agent_ref: String,
    pub content: String,
    pub status: MessageStatus,
    pub sent_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub event_id: String,
    #[serde(rename = "ref", default)]
    pub local_ref: String,
    pub agent_id: String,
    #[serde(default)]
    pub agent_ref: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub side_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_ref: Option<String>,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub sequence: u64,
    pub agent_id: String,
    #[serde(default)]
    pub agent_ref: String,
    pub agent_name: String,
    pub side_id: Option<String>,
    #[serde(default)]
    pub side_ref: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub event_type: String,
    pub priority: u8,
    pub status: AgentStatus,
    pub summary: String,
    #[serde(default)]
    pub acknowledged: bool,
}

fn apply_event_activity(
    activity: &mut ActivityTelemetry,
    event_type: &str,
    data: &Value,
    at: DateTime<Utc>,
) {
    activity.last_event_at = Some(at);
    match event_type {
        "reasoning" | "assistant_message" | "tool_call" => activity.last_model_event_at = Some(at),
        "tool_result" => activity.last_tool_event_at = Some(at),
        _ => {}
    }
    let meaningful = match event_type {
        "tool_call" => data.get("name").and_then(Value::as_str) != Some("write_stdin"),
        "tool_result" if data.get("name").and_then(Value::as_str) == Some("write_stdin") => {
            let result = data.get("result").unwrap_or(&Value::Null);
            result.get("status").and_then(Value::as_str) == Some("completed")
                || result
                    .get("output")
                    .and_then(Value::as_str)
                    .is_some_and(|output| !output.is_empty())
        }
        "lifecycle" => data.get("reason").and_then(Value::as_str) != Some("deadline_updated"),
        _ => true,
    };
    if meaningful {
        activity.last_progress_at = Some(at);
    }
}

#[derive(Debug, Clone)]
pub struct InboxFilter {
    pub limit: usize,
    pub offset: usize,
    pub minimum_priority: u8,
    pub agent_id: Option<String>,
    pub include_acknowledged: bool,
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
    state_root: PathBuf,
    agents_root: PathBuf,
    sides_root: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl Store {
    pub fn new(paths: &Paths) -> Result<Self> {
        ensure_private_dir(&paths.state_dir)?;
        ensure_private_dir(&paths.agents_dir())?;
        ensure_private_dir(&paths.sides_dir())?;
        let store = Self {
            state_root: paths.state_dir.clone(),
            agents_root: paths.agents_dir(),
            sides_root: paths.sides_dir(),
            write_lock: Arc::new(Mutex::new(())),
        };
        store.migrate_activity_metadata()?;
        store.migrate_local_references()?;
        Ok(store)
    }

    fn ref_sequence_path(&self, kind: &str) -> PathBuf {
        self.state_root.join(format!("{kind}-ref-sequence"))
    }

    fn next_local_ref_locked(&self, kind: &str, prefix: &str) -> Result<String> {
        let path = self.ref_sequence_path(kind);
        let current = if path.exists() {
            fs::read_to_string(&path)?.trim().parse::<u64>()?
        } else {
            0
        };
        let next = current
            .checked_add(1)
            .context("local reference sequence exhausted")?;
        write_private_atomic(&path, next.to_string().as_bytes())?;
        Ok(format!("{prefix}_{next}"))
    }

    pub fn allocate_agent_ref(&self) -> Result<String> {
        let _guard = self.write_lock.lock().unwrap();
        self.next_local_ref_locked("agent", "a")
    }

    pub fn allocate_side_ref(&self) -> Result<String> {
        let _guard = self.write_lock.lock().unwrap();
        self.next_local_ref_locked("side", "s")
    }

    fn migrate_local_references(&self) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        let mut agents = fs::read_dir(&self.agents_root)?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("metadata.json"))
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        agents.sort();
        for path in agents {
            let mut meta: AgentMetadata = serde_json::from_slice(&fs::read(&path)?)?;
            if meta.local_ref.is_empty() {
                meta.local_ref = self.next_local_ref_locked("agent", "a")?;
                self.save_metadata(&meta)?;
            }
            let mut messages = self.read_messages(&meta.id)?;
            let mut changed = false;
            for message in &mut messages {
                if message.local_ref.is_empty() {
                    message.local_ref = self.next_local_ref_locked("message", "m")?;
                    changed = true;
                }
                if message.agent_ref.is_empty() {
                    message.agent_ref = meta.local_ref.clone();
                    changed = true;
                }
            }
            if changed {
                self.save_messages(&meta.id, &messages)?;
            }
            self.migrate_event_file(&self.events_path(&meta.id), &meta.local_ref, None)?;
        }
        let mut sides = fs::read_dir(&self.sides_root)?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("metadata.json"))
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        sides.sort();
        for path in sides {
            let mut meta: SideMetadata = serde_json::from_slice(&fs::read(&path)?)?;
            let agent = self.load_metadata(&meta.agent_id)?;
            let mut changed = false;
            if meta.local_ref.is_empty() {
                meta.local_ref = self.next_local_ref_locked("side", "s")?;
                changed = true;
            }
            if meta.agent_ref.is_empty() {
                meta.agent_ref = agent.local_ref.clone();
                changed = true;
            }
            if changed {
                self.save_side_metadata(&meta)?;
            }
            self.migrate_event_file(
                &self.side_dir(&meta.id).join("events.jsonl"),
                &meta.agent_ref,
                Some(&meta.local_ref),
            )?;
        }
        Ok(())
    }

    fn migrate_event_file(
        &self,
        path: &Path,
        agent_ref: &str,
        side_ref: Option<&str>,
    ) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let mut events = Vec::new();
        let mut changed = false;
        for line in BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let mut event: EventRecord = serde_json::from_str(&line)?;
            if event.local_ref.is_empty() {
                event.local_ref = self.next_local_ref_locked("event", "e")?;
                changed = true;
            }
            if event.agent_ref.is_empty() {
                event.agent_ref = agent_ref.to_string();
                changed = true;
            }
            if event.side_ref.is_none() && side_ref.is_some() {
                event.side_ref = side_ref.map(str::to_string);
                changed = true;
            }
            events.push(event);
        }
        if changed {
            let mut body = Vec::new();
            for event in events {
                serde_json::to_writer(&mut body, &event)?;
                body.push(b'\n');
            }
            write_private_atomic(path, &body)?;
        }
        Ok(())
    }

    fn migrate_activity_metadata(&self) -> Result<()> {
        for entry in fs::read_dir(&self.agents_root)? {
            let path = entry?.path().join("metadata.json");
            if !path.exists() {
                continue;
            }
            let mut meta: AgentMetadata = serde_json::from_slice(&fs::read(&path)?)?;
            let needs_migration = meta.last_message_sent_at.is_none()
                || meta.activity.phase_started_at.is_none()
                || meta.activity.last_event_at.is_none()
                || meta.activity.last_progress_at.is_none();
            meta.last_message_sent_at
                .get_or_insert(meta.last_message_at);
            let messages = fs::read(self.messages_path(&meta.id))
                .ok()
                .and_then(|body| serde_json::from_slice::<Vec<MessageRecord>>(&body).ok())
                .unwrap_or_default();
            let delivered = messages
                .iter()
                .filter_map(|message| message.delivered_at)
                .max()
                .map_or(meta.spawned_at, |timestamp| timestamp.max(meta.spawned_at));
            let delivered = Some(delivered);
            if meta.last_message_delivered_at != delivered || needs_migration {
                meta.last_message_delivered_at = delivered;
                migrate_event_activity(&self.events_path(&meta.id), &mut meta.activity)?;
                migrate_terminal_activity(
                    &meta.status,
                    meta.spawned_at,
                    meta.finished_at,
                    meta.stopped_at,
                    meta.failed_at,
                    &mut meta.activity,
                );
                self.save_metadata(&meta)?;
            }
        }
        for entry in fs::read_dir(&self.sides_root)? {
            let path = entry?.path().join("metadata.json");
            if !path.exists() {
                continue;
            }
            let mut meta: SideMetadata = serde_json::from_slice(&fs::read(&path)?)?;
            if meta.activity.phase_started_at.is_some()
                && meta.activity.last_event_at.is_some()
                && meta.activity.last_progress_at.is_some()
            {
                continue;
            }
            migrate_event_activity(
                &self.side_dir(&meta.id).join("events.jsonl"),
                &mut meta.activity,
            )?;
            migrate_terminal_activity(
                &meta.status,
                meta.created_at,
                meta.finished_at,
                meta.stopped_at,
                meta.failed_at,
                &mut meta.activity,
            );
            self.save_side_metadata(&meta)?;
        }
        Ok(())
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
    fn event_sequence_path(&self, id: &str) -> PathBuf {
        self.owner_dir(id).join("event-sequence")
    }
    fn notifications_path(&self) -> PathBuf {
        self.state_root.join("notifications.jsonl")
    }
    fn notification_sequence_path(&self) -> PathBuf {
        self.state_root.join("notification-sequence")
    }
    fn notification_ack_path(&self) -> PathBuf {
        self.state_root.join("notification-acknowledged-through")
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

    pub fn resolve_agent_id(&self, identifier: &str) -> Result<String> {
        if identifier.starts_with("agt_") && self.metadata_path(identifier).exists() {
            return Ok(identifier.to_string());
        }
        let mut matches = Vec::new();
        for entry in fs::read_dir(&self.agents_root)? {
            let path = entry?.path().join("metadata.json");
            let Ok(meta) = fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<AgentMetadata>(&body).ok())
                .ok_or(())
            else {
                continue;
            };
            if meta.local_ref == identifier || meta.name == identifier {
                matches.push(meta.id);
            }
        }
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(coded_error(
                "agent_not_found",
                format!("agent not found: {identifier}"),
                json!({"agent":identifier}),
                false,
            )),
            _ => Err(coded_error(
                "conflict",
                format!("agent name is ambiguous: {identifier}"),
                json!({"agent":identifier,"matches":matches}),
                false,
            )),
        }
    }

    pub fn resolve_side_id(&self, identifier: &str) -> Result<String> {
        if identifier.starts_with("side_")
            && self.side_dir(identifier).join("metadata.json").exists()
        {
            return Ok(identifier.to_string());
        }
        for entry in fs::read_dir(&self.sides_root)? {
            let path = entry?.path().join("metadata.json");
            let Some(meta) = fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<SideMetadata>(&body).ok())
            else {
                continue;
            };
            if meta.local_ref == identifier {
                return Ok(meta.id);
            }
        }
        Err(coded_error(
            "side_not_found",
            format!("side not found: {identifier}"),
            json!({"side":identifier}),
            false,
        ))
    }

    pub fn resolve_message_id(&self, agent_id: &str, identifier: &str) -> Result<String> {
        self.read_messages(agent_id)?
            .into_iter()
            .find(|message| message.id == identifier || message.local_ref == identifier)
            .map(|message| message.id)
            .ok_or_else(|| {
                coded_error(
                    "message_not_found",
                    format!("message not found: {identifier}"),
                    json!({"agent_id":agent_id,"message":identifier}),
                    false,
                )
            })
    }

    pub fn resolve_event_id(&self, owner_id: &str, side: bool, identifier: &str) -> Result<String> {
        if identifier.starts_with("evt_") {
            return Ok(identifier.to_string());
        }
        self.query_events(owner_id, side, &[], None, None, usize::MAX)?
            .into_iter()
            .find(|event| event.local_ref == identifier)
            .map(|event| event.event_id)
            .ok_or_else(|| {
                coded_error(
                    "event_not_found",
                    format!("event not found: {identifier}"),
                    json!({"owner_id":owner_id,"event":identifier}),
                    false,
                )
            })
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
        let agent = self.load_metadata(id)?;
        let message = MessageRecord {
            kind: "message".into(),
            id: format!("msg_{}", ulid::Ulid::new()),
            local_ref: self.next_local_ref_locked("message", "m")?,
            agent_id: id.into(),
            agent_ref: agent.local_ref,
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
        meta.last_message_sent_at = Some(message.sent_at);
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
        if status == MessageStatus::Delivered {
            let mut meta = self.load_metadata(id)?;
            meta.last_message_delivered_at = Some(now);
            self.save_metadata(&meta)?;
        }
        Ok(result)
    }

    pub fn has_message_event(&self, id: &str, message_id: &str) -> Result<bool> {
        let path = self.events_path(id);
        let file = fs::File::open(&path).with_context(|| format!("agent not found: {id}"))?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: EventRecord = serde_json::from_str(&line)?;
            if event.event_type == "user_message"
                && event.data.get("message_id").and_then(Value::as_str) == Some(message_id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn append_event(&self, id: &str, event_type: &str, data: Value) -> Result<EventRecord> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let _guard = self.write_lock.lock().unwrap();
        let sequence = self.next_sequence(&self.events_path(id), &self.event_sequence_path(id))?;
        let agent = self.load_metadata(id)?;
        let event = EventRecord {
            event_id: format!("evt_{}", ulid::Ulid::new()),
            local_ref: self.next_local_ref_locked("event", "e")?,
            agent_id: id.to_string(),
            agent_ref: agent.local_ref,
            side_id: None,
            side_ref: None,
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
        apply_event_activity(&mut meta.activity, event_type, &event.data, event.timestamp);
        self.save_metadata(&meta)?;
        Ok(event)
    }

    pub fn update_agent_activity(
        &self,
        id: &str,
        update: impl FnOnce(&mut ActivityTelemetry),
    ) -> Result<AgentMetadata> {
        let _guard = self.write_lock.lock().unwrap();
        let mut meta = self.load_metadata(id)?;
        update(&mut meta.activity);
        self.save_metadata(&meta)?;
        Ok(meta)
    }

    pub fn update_side_activity(
        &self,
        id: &str,
        update: impl FnOnce(&mut ActivityTelemetry),
    ) -> Result<SideMetadata> {
        let _guard = self.write_lock.lock().unwrap();
        let mut meta = self.load_side_metadata(id)?;
        update(&mut meta.activity);
        self.save_side_metadata(&meta)?;
        Ok(meta)
    }

    fn next_sequence(&self, journal: &Path, counter: &Path) -> Result<u64> {
        let current = if counter.exists() {
            fs::read_to_string(counter)?.trim().parse::<u64>()?
        } else if journal.exists() {
            BufReader::new(fs::File::open(journal)?)
                .lines()
                .map_while(Result::ok)
                .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
                .filter_map(|value| value.get("sequence").and_then(Value::as_u64))
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        let next = current.saturating_add(1);
        write_private_atomic(counter, next.to_string().as_bytes())?;
        Ok(next)
    }

    pub fn append_notification(
        &self,
        owner_id: &str,
        event_type: &str,
        priority: u8,
        status: AgentStatus,
        summary: impl AsRef<str>,
    ) -> Result<NotificationRecord> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        if !(1..=5).contains(&priority) {
            bail!("notification priority must be from 1 through 5");
        }
        let _guard = self.write_lock.lock().unwrap();
        let (agent_id, agent_ref, agent_name, side_id, side_ref) = if owner_id.starts_with("side_")
        {
            let side = self.load_side_metadata(owner_id)?;
            let parent = self.load_metadata(&side.agent_id)?;
            (
                side.agent_id,
                parent.local_ref,
                parent.name,
                Some(owner_id.to_string()),
                Some(side.local_ref),
            )
        } else {
            let agent = self.load_metadata(owner_id)?;
            (agent.id, agent.local_ref, agent.name, None, None)
        };
        let path = self.notifications_path();
        let sequence = self.next_sequence(&path, &self.notification_sequence_path())?;
        let notification = NotificationRecord {
            kind: "notification".into(),
            id: format!("ntf_{}", ulid::Ulid::new()),
            sequence,
            agent_id,
            agent_ref,
            agent_name,
            side_id,
            side_ref,
            timestamp: Utc::now(),
            event_type: event_type.to_string(),
            priority,
            status,
            summary: truncate_chars(summary.as_ref(), 5_000),
            acknowledged: false,
        };
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path)?;
        serde_json::to_writer(&mut file, &notification)?;
        file.write_all(b"\n")?;
        file.flush()?;
        if sequence > 10_000 && sequence % 1_000 == 0 {
            self.compact_notifications_locked()?;
        }
        Ok(notification)
    }

    pub fn list_notifications(&self, filter: &InboxFilter) -> Result<Vec<NotificationRecord>> {
        let path = self.notifications_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut retained = VecDeque::with_capacity(10_000);
        for line in BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let mut notification: NotificationRecord = serde_json::from_str(&line)?;
            if notification.agent_ref.is_empty()
                && let Ok(agent) = self.load_metadata(&notification.agent_id)
            {
                notification.agent_ref = agent.local_ref;
            }
            if notification.side_ref.is_none()
                && let Some(side_id) = notification.side_id.as_deref()
                && let Ok(side) = self.load_side_metadata(side_id)
            {
                notification.side_ref = Some(side.local_ref);
            }
            if retained.len() == 10_000 {
                retained.pop_front();
            }
            retained.push_back(notification);
        }
        let acknowledged_through = self.notification_acknowledged_through()?;
        Ok(retained
            .into_iter()
            .rev()
            .map(|mut notification| {
                notification.acknowledged = notification.sequence <= acknowledged_through;
                notification
            })
            .filter(|notification| filter.include_acknowledged || !notification.acknowledged)
            .filter(|notification| notification.priority >= filter.minimum_priority)
            .filter(|notification| {
                filter
                    .agent_id
                    .as_ref()
                    .is_none_or(|id| &notification.agent_id == id)
            })
            .skip(filter.offset)
            .take(filter.limit)
            .collect())
    }

    pub fn notification_acknowledged_through(&self) -> Result<u64> {
        let path = self.notification_ack_path();
        if !path.exists() {
            return Ok(0);
        }
        Ok(fs::read_to_string(path)?.trim().parse()?)
    }

    pub fn acknowledge_notifications(&self, identifier: &str) -> Result<Value> {
        let _guard = self.write_lock.lock().unwrap();
        let sequence = if let Ok(sequence) = identifier.parse::<u64>() {
            sequence
        } else {
            self.read_all_notifications()?
                .into_iter()
                .find(|notification| notification.id == identifier)
                .map(|notification| notification.sequence)
                .ok_or_else(|| {
                    coded_error(
                        "notification_not_found",
                        format!("notification not found: {identifier}"),
                        json!({"notification":identifier}),
                        false,
                    )
                })?
        };
        let latest = fs::read_to_string(self.notification_sequence_path())
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if sequence == 0 || sequence > latest {
            return Err(coded_error(
                "invalid_argument",
                "notification acknowledgement sequence is outside the journal",
                json!({"sequence":sequence,"latest_sequence":latest}),
                false,
            ));
        }
        let previous = self.notification_acknowledged_through()?;
        let acknowledged_through = previous.max(sequence);
        write_private_atomic(
            &self.notification_ack_path(),
            acknowledged_through.to_string().as_bytes(),
        )?;
        Ok(json!({
            "type":"inbox_acknowledged",
            "acknowledged_through":acknowledged_through,
            "previous_acknowledged_through":previous,
        }))
    }

    pub fn notifications_after(
        &self,
        sequence: u64,
        minimum_priority: u8,
        agent_id: Option<&str>,
    ) -> Result<Vec<NotificationRecord>> {
        let acknowledged_through = self.notification_acknowledged_through()?;
        let mut notifications = self.read_all_notifications()?;
        notifications.retain(|notification| {
            notification.sequence > sequence
                && notification.sequence > acknowledged_through
                && notification.priority >= minimum_priority
                && agent_id.is_none_or(|id| notification.agent_id == id)
        });
        for notification in &mut notifications {
            notification.acknowledged = false;
        }
        Ok(notifications)
    }

    fn read_all_notifications(&self) -> Result<Vec<NotificationRecord>> {
        let path = self.notifications_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        BufReader::new(fs::File::open(path)?)
            .lines()
            .filter_map(|line| match line {
                Ok(line) if !line.trim().is_empty() => {
                    Some(serde_json::from_str(&line).map_err(Into::into))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error.into())),
            })
            .collect()
    }

    fn compact_notifications_locked(&self) -> Result<()> {
        let notifications = self.list_notifications(&InboxFilter {
            limit: 10_000,
            offset: 0,
            minimum_priority: 1,
            agent_id: None,
            include_acknowledged: true,
        })?;
        let mut body = Vec::new();
        for notification in notifications.into_iter().rev() {
            serde_json::to_writer(&mut body, &notification)?;
            body.push(b'\n');
        }
        write_private_atomic(&self.notifications_path(), &body)
    }

    pub fn query_events(
        &self,
        id: &str,
        side: bool,
        types: &[String],
        after: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EventRecord>> {
        let limit = limit.max(1);
        let path = if side {
            self.load_side_metadata(id)?;
            self.side_dir(id).join("events.jsonl")
        } else {
            self.load_metadata(id)?;
            self.events_path(id)
        };
        let file = fs::File::open(&path).with_context(|| format!("history not found: {id}"))?;
        let mut after_found = after.is_none();
        let mut before_found = before.is_none();
        let mut selected = VecDeque::with_capacity(limit.min(10_000));
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: EventRecord = serde_json::from_str(&line)?;
            if before == Some(event.event_id.as_str()) {
                before_found = true;
                break;
            }
            if !after_found {
                if after == Some(event.event_id.as_str()) {
                    after_found = true;
                }
                continue;
            }
            if !types.is_empty() && !types.iter().any(|kind| kind == &event.event_type) {
                continue;
            }
            if selected.len() == limit {
                selected.pop_front();
            }
            selected.push_back(event);
        }
        if !after_found || !before_found {
            bail!("event cursor not found");
        }
        Ok(selected.into())
    }

    pub fn find_event(&self, id: &str, side: bool, event_id: &str) -> Result<EventRecord> {
        let path = if side {
            self.load_side_metadata(id)?;
            self.side_dir(id).join("events.jsonl")
        } else {
            self.load_metadata(id)?;
            self.events_path(id)
        };
        for line in BufReader::new(fs::File::open(path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: EventRecord = serde_json::from_str(&line)?;
            if event.event_id == event_id {
                return Ok(event);
            }
        }
        bail!("event not found")
    }

    pub fn latest_event_id(&self, id: &str, side: bool) -> Result<Option<String>> {
        Ok(self
            .query_events(id, side, &[], None, None, 1)?
            .last()
            .map(|event| event.event_id.clone()))
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
        if let Some(cursor) = filter.after_cursor.as_deref() {
            let cursor = decode_list_cursor(cursor)?;
            if cursor.sort != filter.sort || cursor.order != filter.order {
                return Err(coded_error(
                    "invalid_argument",
                    "list cursor does not match the requested sort order",
                    json!({"cursor_sort":cursor.sort,"cursor_order":cursor.order,"sort":filter.sort,"order":filter.order}),
                    false,
                ));
            }
            out.retain(|meta| {
                let key = list_cursor_key(meta, &filter.sort);
                if filter.order == "asc" {
                    key > (cursor.value, cursor.id.clone())
                } else {
                    key < (cursor.value, cursor.id.clone())
                }
            });
        }
        Ok(out
            .into_iter()
            .skip(filter.offset)
            .take(filter.limit)
            .collect())
    }

    pub fn list_cursor(&self, meta: &AgentMetadata, sort: &str, order: &str) -> Result<String> {
        let (value, id) = list_cursor_key(meta, sort);
        let cursor = ListCursor {
            sort: sort.into(),
            order: order.into(),
            value,
            id,
        };
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor)?))
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
                meta.deadline_at = None;
                meta.stop_reason = Some("daemon_interrupted".into());
                meta.activity.current_phase = AgentPhase::Stopped;
                meta.activity.phase_started_at = Some(now);
                meta.activity.last_state_change_at = Some(now);
                clear_recovered_request(&mut meta.activity);
                self.save_metadata(&meta)?;
                self.append_event(
                    &meta.id,
                    "lifecycle",
                    json!({"status":"stopped","reason":"daemon_interrupted"}),
                )?;
                self.append_notification(
                    &meta.id,
                    "stopped",
                    3,
                    AgentStatus::Stopped,
                    "Agent stopped because the daemon was interrupted",
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
        let sequence = self.next_sequence(&path, &self.event_sequence_path(id))?;
        let event = EventRecord {
            event_id: format!("evt_{}", ulid::Ulid::new()),
            local_ref: self.next_local_ref_locked("event", "e")?,
            agent_id: meta.agent_id.clone(),
            agent_ref: meta.agent_ref.clone(),
            side_id: Some(id.to_string()),
            side_ref: Some(meta.local_ref.clone()),
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
        apply_event_activity(&mut meta.activity, event_type, &event.data, event.timestamp);
        self.save_side_metadata(&meta)?;
        Ok(event)
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
                meta.activity.current_phase = AgentPhase::Stopped;
                meta.activity.phase_started_at = Some(now);
                meta.activity.last_state_change_at = Some(now);
                clear_recovered_request(&mut meta.activity);
                self.save_side_metadata(&meta)?;
                self.append_side_event(
                    &id,
                    "lifecycle",
                    json!({"status":"stopped","reason":"daemon_interrupted"}),
                )?;
                self.append_notification(
                    &id,
                    "stopped",
                    3,
                    AgentStatus::Stopped,
                    "Side agent stopped because the daemon was interrupted",
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

fn migrate_event_activity(path: &Path, activity: &mut ActivityTelemetry) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for line in BufReader::new(fs::File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<EventRecord>(&line) {
            apply_event_activity(activity, &event.event_type, &event.data, event.timestamp);
        }
    }
    Ok(())
}

fn migrate_terminal_activity(
    status: &AgentStatus,
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    stopped_at: Option<DateTime<Utc>>,
    failed_at: Option<DateTime<Utc>>,
    activity: &mut ActivityTelemetry,
) {
    let (phase, changed_at) = match status {
        AgentStatus::Working => (AgentPhase::Starting, created_at),
        AgentStatus::Finished => (AgentPhase::Finished, finished_at.unwrap_or(created_at)),
        AgentStatus::Stopped => (AgentPhase::Stopped, stopped_at.unwrap_or(created_at)),
        AgentStatus::Failed => (AgentPhase::Failed, failed_at.unwrap_or(created_at)),
    };
    activity.current_phase = phase;
    activity.phase_started_at.get_or_insert(changed_at);
    activity.last_state_change_at.get_or_insert(changed_at);
    activity.last_event_at.get_or_insert(created_at);
    if *status != AgentStatus::Working {
        clear_recovered_request(activity);
    }
}

fn clear_recovered_request(activity: &mut ActivityTelemetry) {
    if activity.last_provider_request_id.is_none() {
        activity.last_provider_request_id = activity.provider_request_id.clone();
    }
    activity.request_started_at = None;
    activity.provider_request_id = None;
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

#[derive(Serialize, Deserialize)]
struct ListCursor {
    sort: String,
    order: String,
    value: Option<(i64, u32)>,
    id: String,
}

fn list_cursor_key(meta: &AgentMetadata, sort: &str) -> (Option<(i64, u32)>, String) {
    let value = match sort {
        "updated_at" => Some(timestamp_key(meta.updated_at)),
        "finished_at" => meta.finished_at.map(timestamp_key),
        _ => Some(timestamp_key(meta.spawned_at)),
    };
    (value, meta.id.clone())
}

fn timestamp_key(time: DateTime<Utc>) -> (i64, u32) {
    (time.timestamp(), time.timestamp_subsec_nanos())
}

fn decode_list_cursor(value: &str) -> Result<ListCursor> {
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| {
        coded_error(
            "invalid_argument",
            "invalid agent list cursor",
            json!({"cursor":value}),
            false,
        )
    })?;
    serde_json::from_slice(&decoded).map_err(|_| {
        coded_error(
            "invalid_argument",
            "invalid agent list cursor",
            json!({"cursor":value}),
            false,
        )
    })
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
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
            local_ref: "a_1".into(),
            name: "Parent".into(),
            dir: root.to_string_lossy().into_owned(),
            mode: AgentMode::Write,
            advisory_readonly: false,
            model: "test".into(),
            status: AgentStatus::Finished,
            spawned_at: now,
            last_message_at: now,
            last_message_sent_at: Some(now),
            last_message_delivered_at: Some(now),
            run_started_at: now,
            updated_at: now,
            finished_at: Some(now),
            stopped_at: None,
            failed_at: None,
            deadline_at: None,
            run_number: 1,
            stop_reason: None,
            last_error: None,
            activity: ActivityTelemetry::new(now),
        };
        store.create(&agent, &ContextSnapshot::default()).unwrap();
        let side = SideMetadata {
            kind: "side".into(),
            id: "side_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            local_ref: "s_1".into(),
            agent_id: agent.id.clone(),
            agent_ref: agent.local_ref.clone(),
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
            activity: ActivityTelemetry::new(now),
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
        let assistant = store
            .append_side_event(&side.id, "assistant_message", json!({"content":"answer"}))
            .unwrap();
        fs::remove_file(store.event_sequence_path(&side.id)).unwrap();
        let migrated = store
            .append_side_event(&side.id, "reasoning", json!({"content":"checked"}))
            .unwrap();
        assert_eq!(migrated.sequence, assistant.sequence + 1);
        let selected = store
            .query_events(
                &side.id,
                true,
                &["assistant_message".into()],
                Some(&event.event_id),
                None,
                1,
            )
            .unwrap();
        assert_eq!(selected[0].event_id, assistant.event_id);
        assert_eq!(store.working_side_count(&agent.id).unwrap(), 1);
        assert_eq!(store.recover_interrupted_sides().unwrap(), 1);
        assert_eq!(
            store.load_side_metadata(&side.id).unwrap().status,
            AgentStatus::Stopped
        );
        let notification = store
            .append_notification(
                &agent.id,
                "milestone",
                2,
                AgentStatus::Finished,
                "x".repeat(6_000),
            )
            .unwrap();
        assert_eq!(notification.summary.chars().count(), 5_000);
        let inbox = store
            .list_notifications(&InboxFilter {
                limit: 1,
                offset: 0,
                minimum_priority: 2,
                agent_id: Some(agent.id.clone()),
                include_acknowledged: false,
            })
            .unwrap();
        assert_eq!(inbox[0].id, notification.id);
        let acknowledgement = store.acknowledge_notifications(&notification.id).unwrap();
        assert_eq!(
            acknowledgement["acknowledged_through"],
            notification.sequence
        );
        assert!(
            store
                .list_notifications(&InboxFilter {
                    limit: 10,
                    offset: 0,
                    minimum_priority: 1,
                    agent_id: Some(agent.id.clone()),
                    include_acknowledged: false,
                })
                .unwrap()
                .is_empty()
        );
        let newer = store
            .append_notification(
                &agent.id,
                "progress",
                1,
                AgentStatus::Finished,
                "new progress",
            )
            .unwrap();
        let followed = store
            .notifications_after(notification.sequence, 1, Some(&agent.id))
            .unwrap();
        assert_eq!(followed.len(), 1);
        assert_eq!(followed[0].id, newer.id);
        store.delete_sides_for_agent(&agent.id).unwrap();
        assert!(!store.side_dir(&side.id).exists());
        assert!(
            !store
                .list_notifications(&InboxFilter {
                    limit: 10,
                    offset: 0,
                    minimum_priority: 1,
                    agent_id: Some(agent.id.clone()),
                    include_acknowledged: true,
                })
                .unwrap()
                .is_empty()
        );
        let mut legacy = store.load_metadata(&agent.id).unwrap();
        legacy.last_message_delivered_at = None;
        store.save_metadata(&legacy).unwrap();
        drop(store);
        let reopened = Store::new(&paths).unwrap();
        assert_eq!(
            reopened
                .load_metadata(&agent.id)
                .unwrap()
                .last_message_delivered_at,
            Some(agent.spawned_at)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn inbox_exposes_only_newest_ten_thousand_records() {
        let root = std::env::temp_dir().join(format!("subagent-inbox-store-{}", ulid::Ulid::new()));
        let paths = Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        };
        let store = Store::new(&paths).unwrap();
        let mut file = fs::File::create(store.notifications_path()).unwrap();
        for sequence in 1..=10_001 {
            serde_json::to_writer(
                &mut file,
                &NotificationRecord {
                    kind: "notification".into(),
                    id: format!("ntf_{sequence:026}"),
                    sequence,
                    agent_id: "agt_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                    agent_ref: "a_1".into(),
                    agent_name: "Agent".into(),
                    side_id: None,
                    side_ref: None,
                    timestamp: Utc::now(),
                    event_type: "progress".into(),
                    priority: 1,
                    status: AgentStatus::Working,
                    summary: sequence.to_string(),
                    acknowledged: false,
                },
            )
            .unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.flush().unwrap();
        let inbox = store
            .list_notifications(&InboxFilter {
                limit: 10_000,
                offset: 0,
                minimum_priority: 1,
                agent_id: None,
                include_acknowledged: true,
            })
            .unwrap();
        assert_eq!(inbox.len(), 10_000);
        assert_eq!(inbox.first().unwrap().sequence, 10_001);
        assert_eq!(inbox.last().unwrap().sequence, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_references_are_per_type_monotonic_and_persistent() {
        let root = std::env::temp_dir().join(format!("subagent-ref-store-{}", ulid::Ulid::new()));
        let paths = Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        };
        let store = Store::new(&paths).unwrap();
        assert_eq!(store.allocate_agent_ref().unwrap(), "a_1");
        assert_eq!(store.allocate_agent_ref().unwrap(), "a_2");
        assert_eq!(store.allocate_side_ref().unwrap(), "s_1");
        assert_eq!(store.next_local_ref_locked("message", "m").unwrap(), "m_1");
        assert_eq!(store.next_local_ref_locked("event", "e").unwrap(), "e_1");
        drop(store);
        let reopened = Store::new(&paths).unwrap();
        assert_eq!(reopened.allocate_agent_ref().unwrap(), "a_3");
        assert_eq!(reopened.allocate_side_ref().unwrap(), "s_2");
        assert_eq!(
            reopened.next_local_ref_locked("message", "m").unwrap(),
            "m_2"
        );
        assert_eq!(reopened.next_local_ref_locked("event", "e").unwrap(), "e_2");
        let _ = fs::remove_dir_all(root);
    }
}
