use crate::{
    config::RuntimeConfig,
    ipc::{AgentMode, coded_error},
    model::{ModelProgress, OpenAiClient, assistant_message},
    store::{
        AgentListItem, AgentMetadata, AgentStatus, ContextSnapshot, MessageRecord, Page,
        SideListItem, SideMetadata, Store, canonical_dir, normalize_agent_name,
    },
    tools::{TerminalManager, ToolRuntime, tool_definitions},
};
use anyhow::Result;
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, watch};

#[derive(Clone)]
pub struct AgentManager {
    cfg: Arc<RuntimeConfig>,
    store: Store,
    active: Arc<Mutex<HashMap<String, AgentControl>>>,
    active_sides: Arc<Mutex<HashMap<String, SideControl>>>,
    stopping_agents: Arc<Mutex<HashSet<String>>>,
    operations: Arc<Mutex<()>>,
    stall_notified: Arc<Mutex<HashMap<String, chrono::DateTime<Utc>>>>,
}

#[derive(Clone)]
struct AgentControl {
    run_number: u64,
    messages: mpsc::UnboundedSender<()>,
    stop: watch::Sender<bool>,
    deadline: Arc<Mutex<Option<chrono::DateTime<Utc>>>>,
    terminals: TerminalManager,
    completed: watch::Receiver<bool>,
}

#[derive(Clone)]
struct SideControl {
    stop: watch::Sender<bool>,
    terminals: TerminalManager,
    completed: watch::Receiver<bool>,
}

impl AgentManager {
    pub fn new(cfg: Arc<RuntimeConfig>, store: Store) -> Self {
        Self {
            cfg,
            store,
            active: Default::default(),
            active_sides: Default::default(),
            stopping_agents: Default::default(),
            operations: Default::default(),
            stall_notified: Default::default(),
        }
    }

    pub fn check_stalls(&self) -> Result<()> {
        let threshold = self.cfg.file.stall_notification_seconds;
        if threshold == 0 {
            return Ok(());
        }
        let now = Utc::now();
        let agent_ids = self
            .active
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in agent_ids {
            let meta = self.store.load_metadata(&id)?;
            let baseline = meta
                .activity
                .last_progress_at
                .unwrap_or(meta.run_started_at);
            self.maybe_publish_stall(
                &id,
                &meta.activity.current_phase,
                meta.activity.retry_count,
                meta.activity.provider_request_id.as_deref(),
                baseline,
                now,
                threshold,
            )?;
        }
        let side_ids = self
            .active_sides
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in side_ids {
            let meta = self.store.load_side_metadata(&id)?;
            let baseline = meta
                .activity
                .last_progress_at
                .unwrap_or(meta.run_started_at);
            self.maybe_publish_stall(
                &id,
                &meta.activity.current_phase,
                meta.activity.retry_count,
                meta.activity.provider_request_id.as_deref(),
                baseline,
                now,
                threshold,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn maybe_publish_stall(
        &self,
        id: &str,
        phase: &crate::store::AgentPhase,
        retry_count: u32,
        provider_request_id: Option<&str>,
        baseline: chrono::DateTime<Utc>,
        now: chrono::DateTime<Utc>,
        threshold: u64,
    ) -> Result<()> {
        if (now - baseline).num_seconds() < threshold as i64 {
            self.stall_notified.lock().unwrap().remove(id);
            return Ok(());
        }
        if self
            .stall_notified
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|notified| *notified >= baseline)
        {
            return Ok(());
        }
        let idle = (now - baseline).num_seconds().max(0);
        let request = provider_request_id
            .map(|value| format!(", provider request {value}"))
            .unwrap_or_default();
        self.store.append_notification(
            id,
            "possible_stall",
            3,
            AgentStatus::Working,
            format!(
                "Possible stall: phase {}, inactive for {idle}s, retry count {retry_count}{request}",
                phase.as_str()
            ),
        )?;
        self.stall_notified
            .lock()
            .unwrap()
            .insert(id.into(), baseline);
        Ok(())
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
        model: Option<String>,
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
        let model = normalize_model(model.as_deref().unwrap_or(&self.cfg.file.model))?;
        let deadline = deadline_from_minutes(wall_time_minutes)?;
        let now = Utc::now();
        let id = format!("agt_{}", ulid::Ulid::new());
        let local_ref = self.store.allocate_agent_ref()?;
        let meta = AgentMetadata {
            kind: "agent".into(),
            id: id.clone(),
            local_ref,
            name,
            dir,
            mode,
            advisory_readonly: mode == AgentMode::Readonly,
            model,
            status: AgentStatus::Working,
            spawned_at: now,
            last_message_at: now,
            last_message_sent_at: Some(now),
            last_message_delivered_at: Some(now),
            run_started_at: now,
            updated_at: now,
            finished_at: None,
            stopped_at: None,
            failed_at: None,
            deadline_at: deadline,
            run_number: 1,
            stop_reason: None,
            last_error: None,
            activity: crate::store::ActivityTelemetry::new(now),
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
        self.store
            .append_notification(&id, "spawned", 1, AgentStatus::Working, "Agent spawned")?;
        self.start_worker(meta.clone(), context)?;
        Ok(meta)
    }

    pub fn send(&self, id: &str, message: String, wall_time_minutes: Option<u64>) -> Result<Value> {
        let _operation = self.operations.lock().unwrap();
        if self.stopping_agents.lock().unwrap().contains(id) {
            return Err(coded_error(
                "conflict",
                "agent is stopping; retry after the stop completes",
                json!({"agent_id":id,"status":"stopping"}),
                true,
            ));
        }
        validate_message(&message)?;
        deadline_from_minutes(wall_time_minutes)?;
        let current = self.store.load_metadata(id)?;
        if current.status == AgentStatus::Working
            && let Some(minutes) = wall_time_minutes
        {
            self.update_time(id, minutes)?;
        }
        let message = self.store.enqueue_message(id, message)?;
        let (current_after_send, resume_state, agent_resumed) =
            if current.status == AgentStatus::Working {
                if let Some(control) = self.active.lock().unwrap().get(id).cloned() {
                    let _ = control.messages.send(());
                }
                (self.store.load_metadata(id)?, "not_needed", false)
            } else if self.has_capacity() {
                (self.resume_pending(id, wall_time_minutes)?, "started", true)
            } else {
                (current, "waiting_for_capacity", false)
            };
        Ok(message_receipt(
            &message,
            &current_after_send,
            resume_state,
            agent_resumed,
        ))
    }

    fn resume_pending(&self, id: &str, wall_time_minutes: Option<u64>) -> Result<AgentMetadata> {
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
        meta.activity.current_phase = crate::store::AgentPhase::ProcessingMessages;
        meta.activity.phase_started_at = Some(now);
        meta.activity.last_state_change_at = Some(now);
        meta.activity.last_progress_at = Some(now);
        clear_active_request(&mut meta.activity);
        self.store.save_metadata(&meta)?;
        self.store.append_event(
            id,
            "lifecycle",
            json!({"status":"working","reason":"resumed","run_number":meta.run_number}),
        )?;
        self.store.append_notification(
            id,
            "resumed",
            1,
            AgentStatus::Working,
            format!("Agent resumed for run {}", meta.run_number),
        )?;
        let context = self.store.load_context(id)?;
        self.start_worker(meta.clone(), context)?;
        Ok(meta)
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
            "ref":meta.local_ref,
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

    pub fn list_verbose_values(&self, filter: &crate::ipc::ListFilter) -> Result<Vec<Value>> {
        self.store
            .list(filter)?
            .into_iter()
            .map(|meta| {
                let working_sides = self.store.working_side_count(&meta.id)?;
                let seconds_since_last_event = meta
                    .activity
                    .last_event_at
                    .map(|at| (Utc::now() - at).num_seconds().max(0));
                let mut value = serde_json::to_value(meta)?;
                let object = value.as_object_mut().unwrap();
                object.insert("type".into(), json!("agent_list_item_verbose"));
                object.insert("working_sides".into(), json!(working_sides));
                object.insert(
                    "seconds_since_last_event".into(),
                    json!(seconds_since_last_event),
                );
                Ok(value)
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
        let (completed_tx, completed_rx) = watch::channel(false);
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
                completed: completed_rx,
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
            completed_tx.send(true).ok();
            let _ = manager.schedule_pending();
        });
        Ok(())
    }

    pub async fn stop(&self, id: &str, reason: &str) -> Result<AgentMetadata> {
        let control = {
            let _operation = self.operations.lock().unwrap();
            if self.stopping_agents.lock().unwrap().contains(id) {
                return Err(coded_error(
                    "conflict",
                    "agent is already stopping",
                    json!({"agent_id":id,"status":"stopping"}),
                    true,
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
            self.stopping_agents.lock().unwrap().insert(id.to_string());
            if let Err(error) = self.store.cancel_pending_messages(id) {
                self.stopping_agents.lock().unwrap().remove(id);
                return Err(error);
            }
            control.stop.send(true).ok();
            control
        };
        control.terminals.terminate_all().await;
        let mut completed = control.completed.clone();
        let cleanup = if !*completed.borrow() {
            completed.changed().await.map_err(|_| {
                coded_error(
                    "internal_error",
                    "agent worker ended without completing stop cleanup",
                    json!({"agent_id":id}),
                    true,
                )
            })
        } else {
            Ok(())
        };
        let _operation = self.operations.lock().unwrap();
        let result = cleanup.and_then(|()| {
            self.store.cancel_pending_messages(id)?;
            mark_stopped(&self.store, id, reason)?;
            self.store.load_metadata(id)
        });
        self.stopping_agents.lock().unwrap().remove(id);
        result
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
        model: Option<String>,
        wall_time_minutes: Option<u64>,
    ) -> Result<Value> {
        let _operation = self.operations.lock().unwrap();
        validate_message(&message)?;
        let meta = self.store.load_metadata(id)?;
        let model = normalize_model(model.as_deref().unwrap_or(&meta.model))?;
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
        let side_system_prompt = "You are a persistent, strictly non-modifying side agent branching from a parent coding-agent conversation. Your only goal is to answer the new side question using the inherited context. If the answer is not already established, inspect files, search with glob or grep, run non-mutating Bash commands such as rg or grep, poll terminals, read stored output, or view images. Do not create, edit, delete, rename, or otherwise modify files, repositories, processes, configuration, or external state. Work independently: your messages and tool activity are recorded in the Side run but are not added to the parent's transcript. Use notify for meaningful progress, milestones, questions requiring input, or blockers; do not notify for every tool call. Return a focused answer as soon as the question is resolved.";
        context.messages.push(json!({
            "role":"system",
            "content":side_system_prompt
        }));
        context
            .messages
            .push(json!({"role":"user","content":message.clone()}));
        let deadline = deadline_from_minutes(wall_time_minutes)?;
        let side_id = format!("side_{}", ulid::Ulid::new());
        let side_ref = self.store.allocate_side_ref()?;
        let agent_ref = meta.local_ref.clone();
        let now = Utc::now();
        let side = SideMetadata {
            kind: "side".into(),
            id: side_id.clone(),
            local_ref: side_ref.clone(),
            agent_id: id.into(),
            agent_ref: agent_ref.clone(),
            status: AgentStatus::Working,
            question: message.clone(),
            answer: None,
            model,
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
            activity: crate::store::ActivityTelemetry::new(now),
        };
        self.store.create_side(&side, &context)?;
        self.store.append_side_event(
            &side_id,
            "system_message",
            json!({"content":side_system_prompt}),
        )?;
        self.store.append_side_event(
            &side_id,
            "user_message",
            json!({"content":message,"source":"create"}),
        )?;
        self.store.append_notification(
            &side_id,
            "spawned",
            1,
            AgentStatus::Working,
            "Side agent spawned",
        )?;
        let terminals = TerminalManager::default();
        let (stop_tx, stop_rx) = watch::channel(false);
        let (completed_tx, completed_rx) = watch::channel(false);
        self.active_sides.lock().unwrap().insert(
            side_id.clone(),
            SideControl {
                stop: stop_tx,
                terminals: terminals.clone(),
                completed: completed_rx,
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
            completed_tx.send(true).ok();
        });
        Ok(
            json!({"type":"side_created","id":side_id,"ref":side_ref,"agent_id":id,"agent_ref":agent_ref,"status":"working","created_at":now}),
        )
    }

    pub fn list_sides(
        &self,
        agent_id: &str,
        statuses: &[String],
        limit: usize,
        offset: usize,
        after_cursor: Option<&str>,
    ) -> Result<Page<SideListItem>> {
        let page = self
            .store
            .list_sides_page(agent_id, statuses, limit, offset, after_cursor)?;
        Ok(Page {
            items: page.items.into_iter().map(SideListItem::from).collect(),
            next_cursor: page.next_cursor,
        })
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
        let mut completed = control.completed.clone();
        if !*completed.borrow() {
            completed.changed().await.map_err(|_| {
                coded_error(
                    "internal_error",
                    "Side worker ended without completing stop cleanup",
                    json!({"side_id":id}),
                    true,
                )
            })?;
        }
        mark_side_stopped(&self.store, id, reason)?;
        self.store.load_side_metadata(id)
    }

    pub async fn delete_agent(&self, id: &str) -> Result<Value> {
        let meta = self.store.load_metadata(id)?;
        if meta.status == AgentStatus::Working {
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
        Ok(json!({"type":"agent_deleted","id":id,"ref":meta.local_ref}))
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
    set_side_phase(
        &store,
        &side.id,
        crate::store::AgentPhase::ProcessingMessages,
    )?;
    let client = OpenAiClient::new(
        cfg.api_key.clone(),
        cfg.file.base_url.clone(),
        side.model.clone(),
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
        begin_side_model_request(&store, &side.id)?;
        let mut last_activity_write = None;
        let completion = client.complete_observed(&context.messages, &defs, |progress| {
            record_side_model_progress(&store, &side.id, progress, &mut last_activity_write)
        });
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
        complete_side_model_request(&store, &side.id)?;
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
            side.activity.current_phase = crate::store::AgentPhase::Finished;
            side.activity.phase_started_at = Some(now);
            side.activity.last_state_change_at = Some(now);
            clear_active_request(&mut side.activity);
            store.save_side_metadata(&side)?;
            store.append_side_event(&side.id, "lifecycle", json!({"status":"finished"}))?;
            store.append_notification(
                &side.id,
                "finished",
                2,
                AgentStatus::Finished,
                side.answer
                    .as_deref()
                    .filter(|answer| !answer.is_empty())
                    .unwrap_or("Side agent finished"),
            )?;
            return Ok(());
        }
        let mut image_messages = Vec::new();
        for call in turn.tool_calls {
            let phase = if call.function.name == "write_stdin" {
                crate::store::AgentPhase::WaitingTerminal
            } else {
                crate::store::AgentPhase::ExecutingTool
            };
            set_side_phase(&store, &side.id, phase)?;
            side = store.load_side_metadata(&side.id)?;
            side.tool_calls += 1;
            store.save_side_metadata(&side)?;
            store.append_side_event(&side.id, "tool_call", json!({"tool_call_id":call.id,"name":call.function.name,"arguments":call.function.arguments}))?;
            let result = tokio::select! {
                result = runtime.execute(&call) => result,
                changed = stop.changed() => {
                    if changed.is_ok() && *stop.borrow() {
                        runtime.terminals.terminate_all().await;
                        return Ok(());
                    }
                    runtime.execute(&call).await
                }
            };
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
            set_side_phase(
                &store,
                &side.id,
                crate::store::AgentPhase::ProcessingMessages,
            )?;
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
    set_agent_phase(
        &store,
        &meta.id,
        crate::store::AgentPhase::ProcessingMessages,
    )?;
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
        begin_agent_model_request(&store, &meta.id)?;
        let mut last_activity_write = None;
        let turn_future = client.complete_observed(&request_messages, &defs, |progress| {
            record_agent_model_progress(&store, &meta.id, progress, &mut last_activity_write)
        });
        tokio::pin!(turn_future);
        let turn = loop {
            tokio::select! {
                result=&mut turn_future=>break result?,
                changed=stop_rx.changed()=>{if changed.is_ok()&&*stop_rx.borrow(){terminals.terminate_all().await;return Ok(())}},
                _=tokio::time::sleep(std::time::Duration::from_secs(1))=>{if deadline.lock().unwrap().is_some_and(|d|Utc::now()>=d){terminals.terminate_all().await;mark_stopped(&store,&meta.id,"wall_time")?;return Ok(())}}
            }
        };
        complete_agent_model_request(&store, &meta.id)?;
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
            mark_finished(&store, &meta.id, &turn.content)?;
            return Ok(());
        }
        let mut image_messages = Vec::new();
        for call in turn.tool_calls {
            let phase = if call.function.name == "write_stdin" {
                crate::store::AgentPhase::WaitingTerminal
            } else {
                crate::store::AgentPhase::ExecutingTool
            };
            set_agent_phase(&store, &meta.id, phase)?;
            store.append_event(&meta.id,"tool_call",json!({"tool_call_id":call.id,"name":call.function.name,"arguments":call.function.arguments}))?;
            let result = tokio::select! {
                result = runtime.execute(&call) => result,
                changed = stop_rx.changed() => {
                    if changed.is_ok() && *stop_rx.borrow() {
                        terminals.terminate_all().await;
                        return Ok(());
                    }
                    runtime.execute(&call).await
                }
            };
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
            set_agent_phase(
                &store,
                &meta.id,
                crate::store::AgentPhase::ProcessingMessages,
            )?;
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

fn set_agent_phase(store: &Store, id: &str, phase: crate::store::AgentPhase) -> Result<()> {
    let now = Utc::now();
    store.update_agent_activity(id, |activity| {
        activity.current_phase = phase;
        activity.phase_started_at = Some(now);
        activity.request_started_at = None;
        activity.provider_request_id = None;
    })?;
    Ok(())
}

fn set_side_phase(store: &Store, id: &str, phase: crate::store::AgentPhase) -> Result<()> {
    let now = Utc::now();
    store.update_side_activity(id, |activity| {
        activity.current_phase = phase;
        activity.phase_started_at = Some(now);
        activity.request_started_at = None;
        activity.provider_request_id = None;
    })?;
    Ok(())
}

fn begin_agent_model_request(store: &Store, id: &str) -> Result<()> {
    let now = Utc::now();
    store.update_agent_activity(id, |activity| {
        activity.current_phase = crate::store::AgentPhase::RequestingModel;
        activity.phase_started_at = Some(now);
        activity.request_started_at = Some(now);
        activity.last_provider_activity_at = None;
        activity.provider_request_id = None;
        activity.retry_count = 0;
    })?;
    Ok(())
}

fn begin_side_model_request(store: &Store, id: &str) -> Result<()> {
    let now = Utc::now();
    store.update_side_activity(id, |activity| {
        activity.current_phase = crate::store::AgentPhase::RequestingModel;
        activity.phase_started_at = Some(now);
        activity.request_started_at = Some(now);
        activity.last_provider_activity_at = None;
        activity.provider_request_id = None;
        activity.retry_count = 0;
    })?;
    Ok(())
}

fn complete_agent_model_request(store: &Store, id: &str) -> Result<()> {
    let now = Utc::now();
    store.update_agent_activity(id, |activity| {
        activity.current_phase = crate::store::AgentPhase::ProcessingMessages;
        activity.phase_started_at = Some(now);
        activity.last_model_event_at = Some(now);
        activity.last_provider_activity_at = Some(now);
        activity.last_provider_request_id = activity.provider_request_id.take();
        activity.request_started_at = None;
        activity.last_progress_at = Some(now);
    })?;
    Ok(())
}

fn complete_side_model_request(store: &Store, id: &str) -> Result<()> {
    let now = Utc::now();
    store.update_side_activity(id, |activity| {
        activity.current_phase = crate::store::AgentPhase::ProcessingMessages;
        activity.phase_started_at = Some(now);
        activity.last_model_event_at = Some(now);
        activity.last_provider_activity_at = Some(now);
        activity.last_provider_request_id = activity.provider_request_id.take();
        activity.request_started_at = None;
        activity.last_progress_at = Some(now);
    })?;
    Ok(())
}

fn record_agent_model_progress(
    store: &Store,
    id: &str,
    progress: ModelProgress,
    last_activity_write: &mut Option<chrono::DateTime<Utc>>,
) -> Result<()> {
    record_model_progress(false, store, id, progress, last_activity_write)
}

fn record_side_model_progress(
    store: &Store,
    id: &str,
    progress: ModelProgress,
    last_activity_write: &mut Option<chrono::DateTime<Utc>>,
) -> Result<()> {
    record_model_progress(true, store, id, progress, last_activity_write)
}

fn record_model_progress(
    side: bool,
    store: &Store,
    id: &str,
    progress: ModelProgress,
    last_activity_write: &mut Option<chrono::DateTime<Utc>>,
) -> Result<()> {
    let now = Utc::now();
    let should_write = match progress {
        ModelProgress::ProviderActivity => {
            last_activity_write.is_none_or(|at| (now - at).num_seconds() >= 5)
        }
        _ => true,
    };
    if !should_write {
        return Ok(());
    }
    let update = |activity: &mut crate::store::ActivityTelemetry| match &progress {
        ModelProgress::AttemptStarted { retry_count } => {
            activity.current_phase = crate::store::AgentPhase::RequestingModel;
            activity.phase_started_at = Some(now);
            activity.request_started_at = Some(now);
            activity.provider_request_id = None;
            activity.retry_count = *retry_count;
            activity.last_progress_at = Some(now);
        }
        ModelProgress::ResponseStarted {
            provider_request_id,
        } => {
            activity.provider_request_id = provider_request_id.clone();
            activity.last_provider_request_id = provider_request_id.clone();
            activity.last_provider_activity_at = Some(now);
            activity.last_progress_at = Some(now);
        }
        ModelProgress::ProviderActivity => {
            activity.last_provider_activity_at = Some(now);
            activity.last_progress_at = Some(now);
        }
        ModelProgress::RetryScheduled { retry_count } => {
            activity.current_phase = crate::store::AgentPhase::RetryingModel;
            activity.phase_started_at = Some(now);
            activity.retry_count = *retry_count;
            activity.last_progress_at = Some(now);
        }
    };
    if side {
        store.update_side_activity(id, update)?;
    } else {
        store.update_agent_activity(id, update)?;
    }
    if matches!(
        progress,
        ModelProgress::ProviderActivity | ModelProgress::ResponseStarted { .. }
    ) {
        *last_activity_write = Some(now);
    }
    Ok(())
}

fn message_receipt(
    message: &MessageRecord,
    meta: &AgentMetadata,
    resume_state: &str,
    agent_resumed: bool,
) -> Value {
    json!({
        "type":"message_sent",
        "message_id":message.id,
        "message_ref":message.local_ref,
        "agent_id":message.agent_id,
        "agent_ref":message.agent_ref,
        "status":"queued",
        "sent_at":message.sent_at,
        "agent_resumed":agent_resumed,
        "run_number":meta.run_number,
        "agent_status":meta.status,
        "resume_state":resume_state,
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
    json!({"role":"system","content":format!("You are a background coding agent managed by the subagent daemon. Working directory: {}. {} Use dedicated file tools before shell equivalents. Long-running commands return terminal IDs; poll them with write_stdin. Use notify for meaningful progress, milestones, questions requiring input, or blockers; do not notify for every tool call. Keep tool output focused.",meta.dir,mode)})
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

fn mark_finished(store: &Store, id: &str, final_message: &str) -> Result<()> {
    let mut m = store.load_metadata(id)?;
    let now = Utc::now();
    m.status = AgentStatus::Finished;
    m.updated_at = now;
    m.finished_at = Some(now);
    m.deadline_at = None;
    m.activity.current_phase = crate::store::AgentPhase::Finished;
    m.activity.phase_started_at = Some(now);
    m.activity.last_state_change_at = Some(now);
    clear_active_request(&mut m.activity);
    store.save_metadata(&m)?;
    store.append_event(id, "lifecycle", json!({"status":"finished"}))?;
    store.append_notification(
        id,
        "finished",
        2,
        AgentStatus::Finished,
        if final_message.is_empty() {
            "Agent finished"
        } else {
            final_message
        },
    )?;
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
    m.activity.current_phase = crate::store::AgentPhase::Stopped;
    m.activity.phase_started_at = Some(now);
    m.activity.last_state_change_at = Some(now);
    clear_active_request(&mut m.activity);
    store.save_metadata(&m)?;
    store.append_event(id, "lifecycle", json!({"status":"stopped","reason":reason}))?;
    store.append_notification(
        id,
        "stopped",
        3,
        AgentStatus::Stopped,
        format!("Agent stopped: {reason}"),
    )?;
    Ok(())
}
fn mark_failed(store: &Store, id: &str, error: String) -> Result<()> {
    let mut m = store.load_metadata(id)?;
    if m.status != AgentStatus::Working {
        return Ok(());
    }
    let now = Utc::now();
    m.status = AgentStatus::Failed;
    m.updated_at = now;
    m.failed_at = Some(now);
    m.last_error = Some(error.clone());
    m.deadline_at = None;
    m.activity.current_phase = crate::store::AgentPhase::Failed;
    m.activity.phase_started_at = Some(now);
    m.activity.last_state_change_at = Some(now);
    clear_active_request(&mut m.activity);
    store.save_metadata(&m)?;
    store.append_event(id, "error", json!({"status":"failed","error":error}))?;
    store.append_notification(id, "failed", 4, AgentStatus::Failed, &error)?;
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
    meta.activity.current_phase = crate::store::AgentPhase::Stopped;
    meta.activity.phase_started_at = Some(now);
    meta.activity.last_state_change_at = Some(now);
    clear_active_request(&mut meta.activity);
    store.save_side_metadata(&meta)?;
    store.append_side_event(id, "lifecycle", json!({"status":"stopped","reason":reason}))?;
    store.append_notification(
        id,
        "stopped",
        3,
        AgentStatus::Stopped,
        format!("Side agent stopped: {reason}"),
    )?;
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
    meta.activity.current_phase = crate::store::AgentPhase::Failed;
    meta.activity.phase_started_at = Some(now);
    meta.activity.last_state_change_at = Some(now);
    clear_active_request(&mut meta.activity);
    store.save_side_metadata(&meta)?;
    store.append_side_event(id, "error", json!({"status":"failed","error":error}))?;
    store.append_notification(id, "failed", 4, AgentStatus::Failed, &error)?;
    Ok(())
}

fn clear_active_request(activity: &mut crate::store::ActivityTelemetry) {
    if activity.last_provider_request_id.is_none() {
        activity.last_provider_request_id = activity.provider_request_id.clone();
    }
    activity.request_started_at = None;
    activity.provider_request_id = None;
}

fn normalize_model(value: &str) -> Result<String> {
    let model = value.trim();
    if model.is_empty() {
        return Err(coded_error(
            "invalid_argument",
            "model must not be empty",
            json!({"field":"model"}),
            false,
        ));
    }
    Ok(model.to_string())
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
