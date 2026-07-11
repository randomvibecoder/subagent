use crate::{
    config::RuntimeConfig,
    ipc::{AgentMode, coded_error},
    model::{OpenAiClient, assistant_message},
    store::{
        AgentListItem, AgentMetadata, AgentStatus, ContextSnapshot, MessageRecord, SideListItem,
        SideMetadata, Store, canonical_dir, normalize_agent_name,
    },
    tools::{TerminalManager, ToolRuntime, tool_definitions},
};
use anyhow::Result;
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
    active_sides: Arc<Mutex<HashMap<String, SideControl>>>,
    operations: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct AgentControl {
    run_number: u64,
    messages: mpsc::UnboundedSender<()>,
    stop: watch::Sender<bool>,
    deadline: Arc<Mutex<Option<chrono::DateTime<Utc>>>>,
    terminals: TerminalManager,
}

#[derive(Clone)]
struct SideControl {
    stop: watch::Sender<bool>,
    terminals: TerminalManager,
}

impl AgentManager {
    pub fn new(cfg: Arc<RuntimeConfig>, store: Store) -> Self {
        Self {
            cfg,
            store,
            active: Default::default(),
            active_sides: Default::default(),
            operations: Default::default(),
        }
    }

    pub fn working_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    pub fn spawn(
        &self,
        dir: String,
        message: String,
        name: String,
        mode: AgentMode,
        wall_time_minutes: Option<u64>,
    ) -> Result<AgentMetadata> {
        let _operation = self.operations.lock().unwrap();
        self.check_capacity()?;
        validate_message(&message)?;
        let name = normalize_agent_name(&name).map_err(|error| {
            coded_error(
                "invalid_argument",
                format!("{error:#}"),
                json!({"field":"name"}),
                false,
            )
        })?;
        self.ensure_name_available(&name, None)?;
        let dir = canonical_dir(&dir)?;
        let deadline = deadline_from_minutes(wall_time_minutes)?;
        let now = Utc::now();
        let id = format!("agt_{}", ulid::Ulid::new());
        let meta = AgentMetadata {
            kind: "agent".into(),
            id: id.clone(),
            name,
            dir,
            mode,
            advisory_readonly: mode == AgentMode::Readonly,
            model: self.cfg.file.model.clone(),
            status: AgentStatus::Working,
            spawned_at: now,
            last_message_at: now,
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
            delivered_message_ids: Vec::new(),
        };
        self.store.create(&meta, &context)?;
        self.store.append_event(
            &id,
            "system_message",
            json!({"content":context.messages[0]["content"]}),
        )?;
        self.store.append_event(
            &id,
            "user_message",
            json!({"content":context.messages[1]["content"],"source":"spawn"}),
        )?;
        self.start_worker(meta.clone(), context)?;
        Ok(meta)
    }

    pub fn send(&self, id: &str, message: String, wall_time_minutes: Option<u64>) -> Result<Value> {
        let _operation = self.operations.lock().unwrap();
        validate_message(&message)?;
        deadline_from_minutes(wall_time_minutes)?;
        let current = self.store.load_metadata(id)?;
        if current.status == AgentStatus::Working
            && let Some(minutes) = wall_time_minutes
        {
            self.update_time(id, minutes)?;
        }
        let message = self.store.enqueue_message(id, message)?;
        if current.status == AgentStatus::Working {
            if let Some(control) = self.active.lock().unwrap().get(id).cloned() {
                let _ = control.messages.send(());
            }
        } else if self.has_capacity() {
            self.resume_pending(id, wall_time_minutes)?;
        }
        Ok(message_receipt(&message))
    }

    fn resume_pending(&self, id: &str, wall_time_minutes: Option<u64>) -> Result<()> {
        self.active.lock().unwrap().remove(id);
        let mut meta = self.store.load_metadata(id)?;
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
        meta.deadline_at = deadline_from_minutes(wall_time_minutes)?;
        self.store.save_metadata(&meta)?;
        self.store.append_event(
            id,
            "lifecycle",
            json!({"status":"working","reason":"resumed","run_number":meta.run_number}),
        )?;
        let context = self.store.load_context(id)?;
        self.start_worker(meta.clone(), context)?;
        Ok(())
    }

    fn has_capacity(&self) -> bool {
        self.cfg.file.max_agents == 0 || self.working_count() < self.cfg.file.max_agents
    }

    fn check_capacity(&self) -> Result<()> {
        let running = self.working_count();
        let max = self.cfg.file.max_agents;
        if max > 0 && running >= max {
            return Err(coded_error(
                "capacity_exceeded",
                format!("max agents reached: {running} working"),
                json!({"working_agents":running,"max_agents":max}),
                true,
            ));
        }
        Ok(())
    }

    fn ensure_name_available(&self, name: &str, except_id: Option<&str>) -> Result<()> {
        let filter = crate::ipc::ListFilter {
            limit: usize::MAX,
            sort: "spawned_at".into(),
            order: "asc".into(),
            ..Default::default()
        };
        if let Some(existing) = self
            .store
            .list(&filter)?
            .into_iter()
            .find(|meta| meta.name == name && except_id != Some(meta.id.as_str()))
        {
            return Err(coded_error(
                "conflict",
                format!("agent name is already in use: {name}"),
                json!({"name":name,"agent_id":existing.id}),
                false,
            ));
        }
        Ok(())
    }

    pub fn rename(&self, id: &str, name: String) -> Result<Value> {
        let _operation = self.operations.lock().unwrap();
        let name = normalize_agent_name(&name).map_err(|error| {
            coded_error(
                "invalid_argument",
                format!("{error:#}"),
                json!({"field":"name"}),
                false,
            )
        })?;
        self.ensure_name_available(&name, Some(id))?;
        let mut meta = self.store.load_metadata(id)?;
        meta.name = name.clone();
        self.store.save_metadata(&meta)?;
        Ok(json!({
            "type":"agent_renamed",
            "id":id,
            "name":name,
            "renamed_at":Utc::now(),
        }))
    }

    pub fn list_items(&self, filter: &crate::ipc::ListFilter) -> Result<Vec<AgentListItem>> {
        self.store
            .list(filter)?
            .into_iter()
            .map(|meta| {
                let count = self.store.working_side_count(&meta.id)?;
                Ok(AgentListItem::from_metadata(meta, count))
            })
            .collect()
    }

    pub fn schedule_pending(&self) -> Result<()> {
        let _operation = self.operations.lock().unwrap();
        let filter = crate::ipc::ListFilter {
            limit: usize::MAX,
            sort: "spawned_at".into(),
            order: "asc".into(),
            ..Default::default()
        };
        for meta in self.store.list(&filter)? {
            if !self.has_capacity() {
                break;
            }
            if meta.status != AgentStatus::Working
                && !self.store.pending_messages(&meta.id)?.is_empty()
            {
                self.resume_pending(&meta.id, None)?;
            }
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
                manager.operations.clone(),
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
            let _ = manager.schedule_pending();
        });
        Ok(())
    }

    pub async fn stop(&self, id: &str, reason: &str) -> Result<AgentMetadata> {
        let control = {
            let _operation = self.operations.lock().unwrap();
            let control = self
                .active
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| {
                    coded_error(
                        "conflict",
                        "agent is not working",
                        json!({"agent_id":id}),
                        false,
                    )
                })?;
            control.stop.send(true).ok();
            self.store.cancel_pending_messages(id)?;
            control
        };
        control.terminals.terminate_all().await;
        mark_stopped(&self.store, id, reason)?;
        self.store.load_metadata(id)
    }

    pub fn cancel_message(&self, id: &str, message_id: &str) -> Result<MessageRecord> {
        let _operation = self.operations.lock().unwrap();
        self.store.cancel_message(id, message_id)
    }

    pub fn update_time(&self, id: &str, minutes: u64) -> Result<AgentMetadata> {
        if !(1..=6000).contains(&minutes) {
            return Err(coded_error(
                "invalid_argument",
                "minutes must be from 1 through 6000",
                json!({"field":"minutes"}),
                false,
            ));
        }
        let control = self
            .active
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                coded_error(
                    "conflict",
                    "agent is not working",
                    json!({"agent_id":id}),
                    false,
                )
            })?;
        let deadline = Utc::now() + ChronoDuration::minutes(minutes as i64);
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

    pub fn create_side(
        &self,
        id: &str,
        message: String,
        wall_time_minutes: Option<u64>,
    ) -> Result<Value> {
        let _operation = self.operations.lock().unwrap();
        validate_message(&message)?;
        let meta = self.store.load_metadata(id)?;
        let working = self.store.working_side_count(id)?;
        if working >= 2 {
            return Err(coded_error(
                "capacity_exceeded",
                "side capacity reached for parent agent",
                json!({"agent_id":id,"working_sides":working,"max_sides_per_agent":2}),
                true,
            ));
        }
        let mut context = self.store.load_context(id)?;
        make_side_snapshot_valid(&mut context);
        compact_context(&mut context, self.cfg.file.context_token_budget);
        let inherited_context_messages = context.messages.len();
        context.messages.push(json!({
            "role":"system",
            "content":"You are a persistent, strictly non-modifying side agent branching from a parent coding-agent conversation. Your only goal is to answer the new side question using the inherited context. If the answer is not already established, inspect files, search with glob or grep, run non-mutating Bash commands such as rg or grep, poll terminals, read stored output, or view images. Do not create, edit, delete, rename, or otherwise modify files, repositories, processes, configuration, or external state. Work independently: your messages and tool activity are recorded in the Side run but are not added to the parent's transcript. Return a focused answer as soon as the question is resolved."
        }));
        context
            .messages
            .push(json!({"role":"user","content":message.clone()}));
        let deadline = deadline_from_minutes(wall_time_minutes)?;
        let side_id = format!("side_{}", ulid::Ulid::new());
        let now = Utc::now();
        let side = SideMetadata {
            kind: "side".into(),
            id: side_id.clone(),
            agent_id: id.into(),
            status: AgentStatus::Working,
            question: message.clone(),
            answer: None,
            model: meta.model.clone(),
            mode: AgentMode::Readonly,
            parent_mode: meta.mode,
            created_at: now,
            run_started_at: now,
            updated_at: now,
            finished_at: None,
            stopped_at: None,
            failed_at: None,
            deadline_at: deadline,
            inherited_context_messages,
            tool_calls: 0,
            stop_reason: None,
            last_error: None,
        };
        self.store.create_side(&side, &context)?;
        self.store.append_side_event(
            &side_id,
            "user_message",
            json!({"content":message,"source":"create"}),
        )?;
        let terminals = TerminalManager::default();
        let (stop_tx, stop_rx) = watch::channel(false);
        self.active_sides.lock().unwrap().insert(
            side_id.clone(),
            SideControl {
                stop: stop_tx,
                terminals: terminals.clone(),
            },
        );
        let manager = self.clone();
        let run_id = side_id.clone();
        tokio::spawn(async move {
            let result = run_side(
                manager.cfg.clone(),
                manager.store.clone(),
                meta,
                side,
                context,
                stop_rx,
                terminals.clone(),
            )
            .await;
            terminals.terminate_all().await;
            if let Err(error) = result {
                let _ = mark_side_failed(&manager.store, &run_id, format!("{error:#}"));
            }
            manager.active_sides.lock().unwrap().remove(&run_id);
        });
        Ok(
            json!({"type":"side_created","id":side_id,"agent_id":id,"status":"working","created_at":now}),
        )
    }

    pub fn list_sides(
        &self,
        agent_id: &str,
        statuses: &[String],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SideListItem>> {
        Ok(self
            .store
            .list_sides(agent_id)?
            .into_iter()
            .filter(|side| {
                statuses.is_empty() || statuses.iter().any(|status| status == side.status.as_str())
            })
            .skip(offset)
            .take(limit)
            .map(SideListItem::from)
            .collect())
    }

    pub async fn stop_side(&self, id: &str, reason: &str) -> Result<SideMetadata> {
        let control = self
            .active_sides
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| {
                coded_error(
                    "conflict",
                    "side is not working",
                    json!({"side_id":id}),
                    false,
                )
            })?;
        control.stop.send(true).ok();
        control.terminals.terminate_all().await;
        mark_side_stopped(&self.store, id, reason)?;
        self.store.load_side_metadata(id)
    }

    pub async fn delete_agent(&self, id: &str) -> Result<Value> {
        if self.store.load_metadata(id)?.status == AgentStatus::Working {
            return Err(coded_error(
                "conflict",
                "cannot delete a working agent",
                json!({"agent_id":id,"status":"working"}),
                false,
            ));
        }
        let side_ids = self
            .store
            .list_sides(id)?
            .into_iter()
            .filter(|side| side.status == AgentStatus::Working)
            .map(|side| side.id)
            .collect::<Vec<_>>();
        for side_id in side_ids {
            self.stop_side(&side_id, "parent_deleted").await?;
        }
        self.store.delete_sides_for_agent(id)?;
        self.store.delete(id)?;
        Ok(json!({"type":"agent_deleted","id":id}))
    }

    pub async fn stop_all(&self, reason: &str) {
        let ids: Vec<_> = self.active.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id, reason).await;
        }
        let side_ids: Vec<_> = self.active_sides.lock().unwrap().keys().cloned().collect();
        for id in side_ids {
            let _ = self.stop_side(&id, reason).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_side(
    cfg: Arc<RuntimeConfig>,
    store: Store,
    parent: AgentMetadata,
    mut side: SideMetadata,
    mut context: ContextSnapshot,
    mut stop: watch::Receiver<bool>,
    terminals: TerminalManager,
) -> Result<()> {
    let client = OpenAiClient::new(
        cfg.api_key.clone(),
        cfg.file.base_url.clone(),
        parent.model.clone(),
    )?;
    let runtime = ToolRuntime {
        agent_id: side.id.clone(),
        cwd: parent.dir.clone().into(),
        mode: AgentMode::Readonly,
        store: store.clone(),
        terminals,
        preview_bytes: cfg.file.tool_output_preview_bytes,
    };
    let defs = tool_definitions(AgentMode::Readonly);
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        compact_context(&mut context, cfg.file.context_token_budget);
        let completion = client.complete(&context.messages, &defs);
        let turn = if let Some(deadline) = side.deadline_at {
            let remaining = (deadline - Utc::now())
                .to_std()
                .unwrap_or(std::time::Duration::ZERO);
            tokio::select! {
                _ = stop.changed() => return Ok(()),
                result = tokio::time::timeout(remaining, completion) => match result {
                    Ok(value) => value?,
                    Err(_) => { mark_side_stopped(&store, &side.id, "wall_time")?; return Ok(()); }
                }
            }
        } else {
            tokio::select! {
                _ = stop.changed() => return Ok(()),
                result = completion => result?,
            }
        };
        if !turn.reasoning.is_empty() {
            store.append_side_event(&side.id, "reasoning", json!({"content":turn.reasoning}))?;
        }
        context.messages.push(assistant_message(&turn));
        store.save_side_context(&side.id, &context)?;
        if turn.tool_calls.is_empty() {
            store.append_side_event(
                &side.id,
                "assistant_message",
                json!({"content":turn.content,"usage":turn.usage}),
            )?;
            side = store.load_side_metadata(&side.id)?;
            if side.status != AgentStatus::Working {
                return Ok(());
            }
            let now = Utc::now();
            side.status = AgentStatus::Finished;
            side.answer = Some(turn.content);
            side.updated_at = now;
            side.finished_at = Some(now);
            side.deadline_at = None;
            store.save_side_metadata(&side)?;
            store.append_side_event(&side.id, "lifecycle", json!({"status":"finished"}))?;
            return Ok(());
        }
        let mut image_messages = Vec::new();
        for call in turn.tool_calls {
            side = store.load_side_metadata(&side.id)?;
            side.tool_calls += 1;
            store.save_side_metadata(&side)?;
            store.append_side_event(&side.id, "tool_call", json!({"tool_call_id":call.id,"name":call.function.name,"arguments":call.function.arguments}))?;
            let result = runtime.execute(&call).await;
            store.append_side_event(
                &side.id,
                "tool_result",
                json!({"tool_call_id":call.id,"name":call.function.name,"result":result.content}),
            )?;
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
        store.save_side_context(&side.id, &context)?;
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

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    cfg: Arc<RuntimeConfig>,
    store: Store,
    meta: AgentMetadata,
    mut context: ContextSnapshot,
    mut message_rx: mpsc::UnboundedReceiver<()>,
    mut stop_rx: watch::Receiver<bool>,
    deadline: Arc<Mutex<Option<chrono::DateTime<Utc>>>>,
    terminals: TerminalManager,
    operations: Arc<Mutex<()>>,
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
        while message_rx.try_recv().is_ok() {}
        {
            let _operation = operations.lock().unwrap();
            deliver_pending_messages(&store, &meta.id, &mut context)?;
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
            while message_rx.try_recv().is_ok() {}
            let delivered = {
                let _operation = operations.lock().unwrap();
                deliver_pending_messages(&store, &meta.id, &mut context)?
            };
            if delivered > 0 {
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

fn deliver_pending_messages(
    store: &Store,
    id: &str,
    context: &mut ContextSnapshot,
) -> Result<usize> {
    let pending = store.pending_messages(id)?;
    for message in &pending {
        if !context.delivered_message_ids.contains(&message.id) {
            context.messages.push(json!({
                "role":"user",
                "content":message.content,
            }));
            context.delivered_message_ids.push(message.id.clone());
            store.save_context(id, context)?;
        }
        if !store.has_message_event(id, &message.id)? {
            store.append_event(
                id,
                "user_message",
                json!({"content":message.content,"source":"send","message_id":message.id}),
            )?;
        }
        store.mark_message_delivered(id, &message.id)?;
    }
    Ok(pending.len())
}

fn message_receipt(message: &MessageRecord) -> Value {
    json!({
        "type":"message_sent",
        "message_id":message.id,
        "agent_id":message.agent_id,
        "status":"queued",
        "sent_at":message.sent_at,
    })
}

fn system_message(meta: &AgentMetadata) -> Value {
    let mode = match meta.mode {
        AgentMode::Readonly => {
            "You are in advisory readonly mode. You must not modify files or system state. Bash is provided only for non-mutating inspection with commands such as rg, grep, find, cat, and sed without -i; never run a mutating command."
        }
        AgentMode::Write => {
            "You may inspect and modify the workspace. Complete the task, verify the result, and stop only when finished."
        }
    };
    json!({"role":"system","content":format!("You are a background coding agent managed by the subagent daemon. Working directory: {}. {} Use dedicated file tools before shell equivalents. Long-running commands return terminal IDs; poll them with write_stdin. Keep tool output focused.",meta.dir,mode)})
}

fn validate_message(m: &str) -> Result<()> {
    if m.trim().is_empty() {
        return Err(coded_error(
            "invalid_argument",
            "message is empty",
            json!({"field":"message"}),
            false,
        ));
    }
    if m.len() > 1024 * 1024 {
        return Err(coded_error(
            "file_too_large",
            "message exceeds 1048576 UTF-8 bytes",
            json!({"field":"message","max_bytes":1048576}),
            false,
        ));
    }
    if m.contains('\0') {
        return Err(coded_error(
            "invalid_argument",
            "message contains NUL",
            json!({"field":"message"}),
            false,
        ));
    }
    Ok(())
}
fn deadline_from_minutes(v: Option<u64>) -> Result<Option<chrono::DateTime<Utc>>> {
    match v {
        None => Ok(None),
        Some(minutes) if (1..=6000).contains(&minutes) => {
            Ok(Some(Utc::now() + ChronoDuration::minutes(minutes as i64)))
        }
        Some(_) => Err(coded_error(
            "invalid_argument",
            "wall time must be from 1 through 6000 minutes",
            json!({"field":"wall_time_minutes"}),
            false,
        )),
    }
}

fn compact_context(context: &mut ContextSnapshot, budget: usize) {
    if estimated_tokens(&context.messages) <= budget {
        return;
    }
    let len = context.messages.len();
    for msg in context.messages.iter_mut().take(len.saturating_sub(8)) {
        if msg.get("role").and_then(Value::as_str) == Some("tool")
            && let Some(obj) = msg.as_object_mut()
        {
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

fn mark_side_stopped(store: &Store, id: &str, reason: &str) -> Result<()> {
    let mut meta = store.load_side_metadata(id)?;
    if meta.status != AgentStatus::Working {
        return Ok(());
    }
    let now = Utc::now();
    meta.status = AgentStatus::Stopped;
    meta.updated_at = now;
    meta.stopped_at = Some(now);
    meta.deadline_at = None;
    meta.stop_reason = Some(reason.into());
    store.save_side_metadata(&meta)?;
    store.append_side_event(id, "lifecycle", json!({"status":"stopped","reason":reason}))?;
    Ok(())
}

fn mark_side_failed(store: &Store, id: &str, error: String) -> Result<()> {
    let mut meta = store.load_side_metadata(id)?;
    if meta.status != AgentStatus::Working {
        return Ok(());
    }
    let now = Utc::now();
    meta.status = AgentStatus::Failed;
    meta.updated_at = now;
    meta.failed_at = Some(now);
    meta.deadline_at = None;
    meta.last_error = Some(error.clone());
    store.save_side_metadata(&meta)?;
    store.append_side_event(id, "error", json!({"status":"failed","error":error}))?;
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
            delivered_message_ids: Vec::new(),
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
    fn deadlines_are_bounded_to_six_thousand_minutes() {
        assert!(deadline_from_minutes(None).unwrap().is_none());
        assert!(deadline_from_minutes(Some(6000)).unwrap().is_some());
        assert!(deadline_from_minutes(Some(0)).is_err());
        assert!(deadline_from_minutes(Some(6001)).is_err());
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
            delivered_message_ids: Vec::new(),
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
            delivered_message_ids: Vec::new(),
        };
        make_side_snapshot_valid(&mut context);
        assert_eq!(context.messages.len(), 3);
    }
}
