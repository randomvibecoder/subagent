use crate::{
    config::{RuntimeConfig, ensure_private_dir},
    ipc::AgentMode,
    model::{OpenAiClient, assistant_message},
    store::{
        AgentMetadata, AgentStatus, ContextSnapshot, Store, canonical_dir, title_from_message,
    },
    tools::{TerminalManager, ToolRuntime, tool_definitions},
};
use anyhow::{Context, Result, bail};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, watch};

#[derive(Clone)]
pub struct AgentManager {
    cfg: Arc<RuntimeConfig>,
    store: Store,
    active: Arc<Mutex<HashMap<String, AgentControl>>>,
}

#[derive(Clone)]
struct AgentControl {
    run_number: u64,
    messages: mpsc::UnboundedSender<String>,
    stop: watch::Sender<bool>,
    deadline: Arc<Mutex<Option<chrono::DateTime<Utc>>>>,
    terminals: TerminalManager,
}

impl AgentManager {
    pub fn new(cfg: Arc<RuntimeConfig>, store: Store) -> Self {
        Self {
            cfg,
            store,
            active: Default::default(),
        }
    }

    pub fn working_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    pub fn spawn(
        &self,
        dir: String,
        message: String,
        title: Option<String>,
        mode: AgentMode,
        wall_time_hours: Option<f64>,
    ) -> Result<AgentMetadata> {
        self.check_capacity()?;
        validate_message(&message)?;
        let dir = canonical_dir(&dir)?;
        let deadline = deadline_from_hours(wall_time_hours)?;
        let now = Utc::now();
        let id = format!("agt_{}", ulid::Ulid::new());
        let meta = AgentMetadata {
            kind: "agent".into(),
            id: id.clone(),
            title: title.unwrap_or_else(|| title_from_message(&message)),
            dir,
            mode,
            advisory_readonly: mode == AgentMode::Readonly,
            model: self.cfg.file.model.clone(),
            status: AgentStatus::Working,
            spawned_at: now,
            run_started_at: now,
            updated_at: now,
            finished_at: None,
            stopped_at: None,
            failed_at: None,
            deadline_at: deadline,
            run_number: 1,
            stop_reason: None,
            last_error: None,
        };
        let context = ContextSnapshot {
            messages: vec![
                system_message(&meta),
                json!({"role":"user","content":message}),
            ],
            compacted_at: None,
        };
        self.store.create(&meta, &context)?;
        self.store.append_event(
            &id,
            "user_message",
            json!({"content":context.messages[1]["content"],"source":"spawn"}),
        )?;
        self.start_worker(meta.clone(), context)?;
        Ok(meta)
    }

    pub fn send(
        &self,
        id: &str,
        message: String,
        wall_time_hours: Option<f64>,
    ) -> Result<AgentMetadata> {
        validate_message(&message)?;
        let current = self.store.load_metadata(id)?;
        if current.status == AgentStatus::Working {
            let control = self.active.lock().unwrap().get(id).cloned().context(
                "agent is marked working but not loaded; restart the daemon to recover it",
            )?;
            if let Some(hours) = wall_time_hours {
                self.update_time(id, hours)?;
            }
            control
                .messages
                .send(message)
                .map_err(|_| anyhow::anyhow!("agent message channel closed"))?;
            return self.store.load_metadata(id);
        }
        self.active.lock().unwrap().remove(id);
        self.check_capacity()?;
        let mut meta = current;
        let now = Utc::now();
        meta.status = AgentStatus::Working;
        meta.run_started_at = now;
        meta.updated_at = now;
        meta.run_number += 1;
        meta.finished_at = None;
        meta.stopped_at = None;
        meta.failed_at = None;
        meta.stop_reason = None;
        meta.last_error = None;
        meta.deadline_at = deadline_from_hours(wall_time_hours)?;
        self.store.save_metadata(&meta)?;
        self.store.append_event(
            id,
            "lifecycle",
            json!({"status":"working","reason":"resumed","run_number":meta.run_number}),
        )?;
        let mut context = self.store.load_context(id)?;
        context
            .messages
            .push(json!({"role":"user","content":message}));
        self.store.save_context(id, &context)?;
        self.store.append_event(
            id,
            "user_message",
            json!({"content":context.messages.last().unwrap()["content"],"source":"send"}),
        )?;
        self.start_worker(meta.clone(), context)?;
        Ok(meta)
    }

    fn check_capacity(&self) -> Result<()> {
        let running = self.working_count();
        let max = self.cfg.file.max_agents;
        if max > 0 && running >= max {
            bail!("max agents reached: {running} working; run 'subagent config set max-agents <x>'")
        }
        Ok(())
    }

    fn start_worker(&self, meta: AgentMetadata, context: ContextSnapshot) -> Result<()> {
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        let terminals = TerminalManager::default();
        let deadline = Arc::new(Mutex::new(meta.deadline_at));
        self.active.lock().unwrap().insert(
            meta.id.clone(),
            AgentControl {
                run_number: meta.run_number,
                messages: message_tx,
                stop: stop_tx,
                deadline: deadline.clone(),
                terminals: terminals.clone(),
            },
        );
        let manager = self.clone();
        let id = meta.id.clone();
        let run_number = meta.run_number;
        let cleanup_terminals = terminals.clone();
        tokio::spawn(async move {
            let outcome = run_worker(
                manager.cfg.clone(),
                manager.store.clone(),
                meta,
                context,
                message_rx,
                stop_rx,
                deadline,
                terminals,
            )
            .await;
            cleanup_terminals.terminate_all().await;
            let mut active = manager.active.lock().unwrap();
            if active
                .get(&id)
                .is_some_and(|control| control.run_number == run_number)
            {
                active.remove(&id);
            }
            drop(active);
            if let Err(e) = outcome {
                let _ = mark_failed(&manager.store, &id, format!("{e:#}"));
            }
        });
        Ok(())
    }

    pub async fn stop(&self, id: &str, reason: &str) -> Result<AgentMetadata> {
        let control = self
            .active
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("agent is not working")?;
        control.stop.send(true).ok();
        control.terminals.terminate_all().await;
        mark_stopped(&self.store, id, reason)?;
        Ok(self.store.load_metadata(id)?)
    }

    pub fn update_time(&self, id: &str, hours: f64) -> Result<AgentMetadata> {
        if !(hours > 0.0 && hours <= 100.0) {
            bail!("hours must be greater than 0 and at most 100")
        }
        let control = self
            .active
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("agent is not working")?;
        let deadline = Utc::now() + ChronoDuration::milliseconds((hours * 3_600_000.0) as i64);
        *control.deadline.lock().unwrap() = Some(deadline);
        let mut meta = self.store.load_metadata(id)?;
        meta.deadline_at = Some(deadline);
        meta.updated_at = Utc::now();
        self.store.save_metadata(&meta)?;
        self.store.append_event(
            id,
            "lifecycle",
            json!({"status":"working","reason":"deadline_updated","deadline_at":deadline}),
        )?;
        Ok(meta)
    }

    pub async fn side(
        &self,
        id: &str,
        message: String,
        wall_time_hours: Option<f64>,
    ) -> Result<Value> {
        validate_message(&message)?;
        let meta = self.store.load_metadata(id)?;
        let mut context = self.store.load_context(id)?;
        make_side_snapshot_valid(&mut context);
        compact_context(&mut context, self.cfg.file.context_token_budget);
        let inherited_context_messages = context.messages.len();
        context.messages.push(json!({
            "role":"system",
            "content":"You are an ephemeral, strictly non-modifying side agent branching from a parent coding-agent conversation. Your only goal is to answer the new side question using the inherited context. If the answer is not already established, inspect files, search with glob or grep, run non-mutating Bash commands such as rg or grep, poll terminals, read stored output, or view images. Do not create, edit, delete, rename, or otherwise modify files, repositories, processes, configuration, or external state. Work independently: your messages and tool activity will not be added to the parent's transcript. Return a focused answer as soon as the question is resolved."
        }));
        context
            .messages
            .push(json!({"role":"user","content":message}));
        let deadline = deadline_from_hours(wall_time_hours)?;
        let side_id = format!("side_{}", ulid::Ulid::new());
        ensure_private_dir(&self.store.outputs_dir(&side_id))?;
        let terminals = TerminalManager::default();
        let result = run_side(
            self.cfg.clone(),
            self.store.clone(),
            &meta,
            &side_id,
            context,
            inherited_context_messages,
            deadline,
            terminals.clone(),
        )
        .await;
        terminals.terminate_all().await;
        let _ = std::fs::remove_dir_all(self.store.agent_dir(&side_id));
        result
    }

    pub async fn stop_all(&self, reason: &str) {
        let ids: Vec<_> = self.active.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id, reason).await;
        }
    }
}

async fn run_side(
    cfg: Arc<RuntimeConfig>,
    store: Store,
    meta: &AgentMetadata,
    side_id: &str,
    mut context: ContextSnapshot,
    inherited_context_messages: usize,
    deadline: Option<chrono::DateTime<Utc>>,
    terminals: TerminalManager,
) -> Result<Value> {
    let client = OpenAiClient::new(
        cfg.api_key.clone(),
        cfg.file.base_url.clone(),
        meta.model.clone(),
    )?;
    let runtime = ToolRuntime {
        agent_id: side_id.to_string(),
        cwd: meta.dir.clone().into(),
        mode: AgentMode::Readonly,
        store,
        terminals,
        preview_bytes: cfg.file.tool_output_preview_bytes,
    };
    let defs = tool_definitions(AgentMode::Readonly);
    let mut tool_calls = 0usize;
    loop {
        if deadline.is_some_and(|value| Utc::now() >= value) {
            bail!("side agent wall time exceeded")
        }
        compact_context(&mut context, cfg.file.context_token_budget);
        let turn = if let Some(deadline) = deadline {
            let remaining = (deadline - Utc::now())
                .to_std()
                .unwrap_or(std::time::Duration::ZERO);
            tokio::time::timeout(remaining, client.complete(&context.messages, &defs))
                .await
                .context("side agent wall time exceeded")??
        } else {
            client.complete(&context.messages, &defs).await?
        };
        context.messages.push(assistant_message(&turn));
        if turn.tool_calls.is_empty() {
            return Ok(json!({
                "type":"side_answer",
                "side_id":side_id,
                "agent_id":meta.id,
                "answer":turn.content,
                "model":meta.model,
                "mode":AgentMode::Readonly,
                "parent_mode":meta.mode,
                "ephemeral":true,
                "inherited_context_messages":inherited_context_messages,
                "tool_calls":tool_calls,
                "usage":turn.usage,
            }));
        }
        tool_calls += turn.tool_calls.len();
        let mut image_messages = Vec::new();
        for call in turn.tool_calls {
            let result = runtime.execute(&call).await;
            context.messages.push(json!({
                "role":"tool",
                "tool_call_id":call.id,
                "content":serde_json::to_string(&result.content)?,
            }));
            if let Some(image) = result.image_message {
                image_messages.push(image);
            }
        }
        context.messages.extend(image_messages);
    }
}

fn make_side_snapshot_valid(context: &mut ContextSnapshot) {
    let Some(index) = context.messages.iter().rposition(|message| {
        message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
    }) else {
        return;
    };
    let expected: Vec<_> = context.messages[index]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .collect();
    let present: Vec<_> = context.messages[index + 1..]
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str))
        .collect();
    if expected.iter().any(|id| !present.contains(id)) {
        context.messages.truncate(index);
    }
}

async fn run_worker(
    cfg: Arc<RuntimeConfig>,
    store: Store,
    meta: AgentMetadata,
    mut context: ContextSnapshot,
    mut message_rx: mpsc::UnboundedReceiver<String>,
    mut stop_rx: watch::Receiver<bool>,
    deadline: Arc<Mutex<Option<chrono::DateTime<Utc>>>>,
    terminals: TerminalManager,
) -> Result<()> {
    let client = OpenAiClient::new(
        cfg.api_key.clone(),
        cfg.file.base_url.clone(),
        meta.model.clone(),
    )?;
    let runtime = ToolRuntime {
        agent_id: meta.id.clone(),
        cwd: meta.dir.clone().into(),
        mode: meta.mode,
        store: store.clone(),
        terminals: terminals.clone(),
        preview_bytes: cfg.file.tool_output_preview_bytes,
    };
    let defs = tool_definitions(meta.mode);
    loop {
        while let Ok(message) = message_rx.try_recv() {
            context
                .messages
                .push(json!({"role":"user","content":message.clone()}));
            store.append_event(
                &meta.id,
                "user_message",
                json!({"content":message,"source":"send"}),
            )?;
        }
        compact_context(&mut context, cfg.file.context_token_budget);
        store.save_context(&meta.id, &context)?;
        if *stop_rx.borrow() {
            terminals.terminate_all().await;
            return Ok(());
        }
        if deadline.lock().unwrap().is_some_and(|d| Utc::now() >= d) {
            terminals.terminate_all().await;
            mark_stopped(&store, &meta.id, "wall_time")?;
            return Ok(());
        }
        let request_messages = context.messages.clone();
        let turn_future = client.complete(&request_messages, &defs);
        tokio::pin!(turn_future);
        let turn = loop {
            tokio::select! {
                result=&mut turn_future=>break result?,
                changed=stop_rx.changed()=>{if changed.is_ok()&&*stop_rx.borrow(){terminals.terminate_all().await;return Ok(())}},
                _=tokio::time::sleep(std::time::Duration::from_secs(1))=>{if deadline.lock().unwrap().is_some_and(|d|Utc::now()>=d){terminals.terminate_all().await;mark_stopped(&store,&meta.id,"wall_time")?;return Ok(())}}
            }
        };
        if !turn.reasoning.is_empty() {
            store.append_event(&meta.id, "reasoning", json!({"content":turn.reasoning}))?;
        }
        if !turn.content.is_empty() {
            store.append_event(
                &meta.id,
                "assistant_message",
                json!({"content":turn.content,"usage":turn.usage}),
            )?;
        }
        context.messages.push(assistant_message(&turn));
        if turn.tool_calls.is_empty() {
            if let Ok(message) = message_rx.try_recv() {
                context
                    .messages
                    .push(json!({"role":"user","content":message.clone()}));
                store.append_event(
                    &meta.id,
                    "user_message",
                    json!({"content":message,"source":"send"}),
                )?;
                continue;
            }
            store.save_context(&meta.id, &context)?;
            terminals.terminate_all().await;
            mark_finished(&store, &meta.id)?;
            return Ok(());
        }
        let mut image_messages = Vec::new();
        for call in turn.tool_calls {
            store.append_event(&meta.id,"tool_call",json!({"tool_call_id":call.id,"name":call.function.name,"arguments":call.function.arguments}))?;
            let result = runtime.execute(&call).await;
            let content = serde_json::to_string(&result.content)?;
            store.append_event(
                &meta.id,
                "tool_result",
                json!({"tool_call_id":call.id,"name":call.function.name,"result":result.content}),
            )?;
            context
                .messages
                .push(json!({"role":"tool","tool_call_id":call.id,"content":content}));
            if let Some(image) = result.image_message {
                image_messages.push(image);
            }
            store.save_context(&meta.id, &context)?;
        }
        context.messages.extend(image_messages);
        store.save_context(&meta.id, &context)?;
    }
}

fn system_message(meta: &AgentMetadata) -> Value {
    let mode = match meta.mode {
        AgentMode::Readonly => {
            "You are in advisory readonly mode. Do not modify files or system state. Bash is available only for non-mutating inspection commands."
        }
        AgentMode::Write => {
            "You may inspect and modify the workspace. Complete the task, verify the result, and stop only when finished."
        }
    };
    json!({"role":"system","content":format!("You are a background coding agent managed by the subagent daemon. Working directory: {}. {} Use dedicated file tools before shell equivalents. Long-running commands return terminal IDs; poll them with write_stdin. Keep tool output focused.",meta.dir,mode)})
}

fn validate_message(m: &str) -> Result<()> {
    if m.trim().is_empty() {
        bail!("message is empty")
    }
    if m.len() > 1024 * 1024 {
        bail!("message exceeds 1 MiB")
    }
    Ok(())
}
fn deadline_from_hours(v: Option<f64>) -> Result<Option<chrono::DateTime<Utc>>> {
    match v {
        None => Ok(None),
        Some(h) if h > 0.0 && h <= 100.0 => Ok(Some(
            Utc::now() + ChronoDuration::milliseconds((h * 3_600_000.0) as i64),
        )),
        Some(_) => bail!("wall time must be greater than 0 and at most 100 hours"),
    }
}

fn compact_context(context: &mut ContextSnapshot, budget: usize) {
    if estimated_tokens(&context.messages) <= budget {
        return;
    }
    let len = context.messages.len();
    for msg in context.messages.iter_mut().take(len.saturating_sub(8)) {
        if msg.get("role").and_then(Value::as_str) == Some("tool") {
            if let Some(obj) = msg.as_object_mut() {
                let compact = obj
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|content| serde_json::from_str::<Value>(content).ok())
                    .and_then(|content| content.get("output_ref").cloned())
                    .map(|output_ref| {
                        json!({"omitted":true,"output_ref":output_ref,"hint":"use read_output"})
                            .to_string()
                    })
                    .unwrap_or_else(|| "[older tool output omitted]".to_string());
                obj.insert("content".into(), Value::String(compact));
            }
        }
    }
    if estimated_tokens(&context.messages) <= budget {
        return;
    }
    if context.messages.len() > 12 {
        let mut keep_from = context.messages.len() - 10;
        if context.messages[keep_from]
            .get("role")
            .and_then(Value::as_str)
            == Some("tool")
        {
            while keep_from > 2 {
                keep_from -= 1;
                if context.messages[keep_from]
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some()
                {
                    break;
                }
            }
        }
        let removed = context.messages.drain(2..keep_from).collect::<Vec<_>>();
        let mut summary = removed
            .iter()
            .filter_map(|m| {
                let role = m.get("role")?.as_str()?;
                let content = m.get("content")?.as_str().unwrap_or("");
                Some(format!(
                    "{role}: {}",
                    content.chars().take(500).collect::<String>()
                ))
            })
            .collect::<Vec<_>>()
            .join("\n");
        summary = summary.chars().take(8_000).collect();
        context.messages.insert(
            2,
            json!({"role":"system","content":format!("Compacted earlier history:\n{summary}")}),
        );
        context.compacted_at = Some(Utc::now());
    }
}

fn estimated_tokens(messages: &[Value]) -> usize {
    serde_json::to_vec(messages)
        .map(|b| b.len() / 4)
        .unwrap_or(0)
}

fn mark_finished(store: &Store, id: &str) -> Result<()> {
    let mut m = store.load_metadata(id)?;
    let now = Utc::now();
    m.status = AgentStatus::Finished;
    m.updated_at = now;
    m.finished_at = Some(now);
    m.deadline_at = None;
    store.save_metadata(&m)?;
    store.append_event(id, "lifecycle", json!({"status":"finished"}))?;
    Ok(())
}
fn mark_stopped(store: &Store, id: &str, reason: &str) -> Result<()> {
    let mut m = store.load_metadata(id)?;
    let now = Utc::now();
    m.status = AgentStatus::Stopped;
    m.updated_at = now;
    m.stopped_at = Some(now);
    m.stop_reason = Some(reason.into());
    m.deadline_at = None;
    store.save_metadata(&m)?;
    store.append_event(id, "lifecycle", json!({"status":"stopped","reason":reason}))?;
    Ok(())
}
fn mark_failed(store: &Store, id: &str, error: String) -> Result<()> {
    let mut m = store.load_metadata(id)?;
    if m.status == AgentStatus::Stopped {
        return Ok(());
    }
    let now = Utc::now();
    m.status = AgentStatus::Failed;
    m.updated_at = now;
    m.failed_at = Some(now);
    m.last_error = Some(error.clone());
    m.deadline_at = None;
    store.save_metadata(&m)?;
    store.append_event(id, "error", json!({"status":"failed","error":error}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_does_not_orphan_tool_results() {
        let mut messages = vec![
            json!({"role":"system","content":"system"}),
            json!({"role":"user","content":"task"}),
        ];
        for index in 0..12 {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call_{index}"),"type":"function","function":{"name":"read","arguments":"{}"}}]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("call_{index}"),"content":"x".repeat(2000)}));
        }
        let mut context = ContextSnapshot {
            messages,
            compacted_at: None,
        };
        compact_context(&mut context, 1000);
        assert!(context.compacted_at.is_some());
        for (index, message) in context.messages.iter().enumerate() {
            if message.get("role").and_then(Value::as_str) == Some("tool") {
                assert!(context.messages[..index].iter().any(|candidate| {
                    candidate
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| {
                            calls
                                .iter()
                                .any(|call| call.get("id") == message.get("tool_call_id"))
                        })
                }));
            }
        }
    }

    #[test]
    fn deadlines_are_bounded_to_one_hundred_hours() {
        assert!(deadline_from_hours(None).unwrap().is_none());
        assert!(deadline_from_hours(Some(100.0)).unwrap().is_some());
        assert!(deadline_from_hours(Some(0.0)).is_err());
        assert!(deadline_from_hours(Some(100.1)).is_err());
    }

    #[test]
    fn side_snapshot_drops_an_incomplete_tool_turn() {
        let mut context = ContextSnapshot {
            messages: vec![
                json!({"role":"system","content":"system"}),
                json!({"role":"user","content":"task"}),
                json!({"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}},
                    {"id":"call_2","type":"function","function":{"name":"grep","arguments":"{}"}}
                ]}),
                json!({"role":"tool","tool_call_id":"call_1","content":"done"}),
            ],
            compacted_at: None,
        };
        make_side_snapshot_valid(&mut context);
        assert_eq!(context.messages.len(), 2);
    }

    #[test]
    fn side_snapshot_keeps_a_complete_tool_turn() {
        let mut context = ContextSnapshot {
            messages: vec![
                json!({"role":"system","content":"system"}),
                json!({"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}}
                ]}),
                json!({"role":"tool","tool_call_id":"call_1","content":"done"}),
            ],
            compacted_at: None,
        };
        make_side_snapshot_valid(&mut context);
        assert_eq!(context.messages.len(), 3);
    }
}
