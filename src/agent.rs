use crate::{
    config::RuntimeConfig,
    ipc::{AgentMode, coded_error},
    model::{ModelProgress, OpenAiClient, assistant_message},
    prompts,
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
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot, watch};

#[derive(Clone)]
pub struct AgentManager {
    cfg: Arc<RuntimeConfig>,
    store: Store,
    active: Arc<Mutex<HashMap<String, AgentControl>>>,
    active_sides: Arc<Mutex<HashMap<String, SideControl>>>,
    stopping_agents: Arc<Mutex<HashSet<String>>>,
    stopping_sides: Arc<Mutex<HashSet<String>>>,
    owner_locks: Arc<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
    operations: Arc<Mutex<()>>,
    stall_notified: Arc<Mutex<HashMap<String, chrono::DateTime<Utc>>>>,
    mutation_gate: Arc<MutationGate>,
}

struct MutationGate {
    accepting: AtomicBool,
    active: AtomicUsize,
    idle: Notify,
}

pub struct MutationPermit {
    gate: Arc<MutationGate>,
}

impl Drop for MutationPermit {
    fn drop(&mut self) {
        if self.gate.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.gate.idle.notify_waiters();
        }
    }
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
            stopping_sides: Default::default(),
            owner_locks: Default::default(),
            operations: Default::default(),
            stall_notified: Default::default(),
            mutation_gate: Arc::new(MutationGate {
                accepting: AtomicBool::new(true),
                active: AtomicUsize::new(0),
                idle: Notify::new(),
            }),
        }
    }

    pub fn begin_mutation(&self) -> Result<MutationPermit> {
        if !self.mutation_gate.accepting.load(Ordering::Acquire) {
            return Err(daemon_stopping_error());
        }
        self.mutation_gate.active.fetch_add(1, Ordering::AcqRel);
        if !self.mutation_gate.accepting.load(Ordering::Acquire) {
            if self.mutation_gate.active.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.mutation_gate.idle.notify_waiters();
            }
            return Err(daemon_stopping_error());
        }
        Ok(MutationPermit {
            gate: self.mutation_gate.clone(),
        })
    }

    fn track_internal_mutation(&self) -> MutationPermit {
        self.mutation_gate.active.fetch_add(1, Ordering::AcqRel);
        MutationPermit {
            gate: self.mutation_gate.clone(),
        }
    }

    pub fn begin_shutdown(&self) {
        self.mutation_gate.accepting.store(false, Ordering::Release);
    }

    pub async fn wait_for_mutations(&self) {
        loop {
            // Register before checking the counter so a final permit cannot notify in
            // the gap between the observation and waiter registration.
            let notified = self.mutation_gate.idle.notified();
            if self.mutation_gate.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn owner_lock(&self, id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.owner_locks.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(id.to_string(), Arc::downgrade(&lock));
        lock
    }

    fn accepting_work(&self) -> bool {
        self.mutation_gate.accepting.load(Ordering::Acquire)
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
            self.maybe_publish_stall(&id, &meta.activity, meta.run_started_at, now, threshold)?;
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
            self.maybe_publish_stall(&id, &meta.activity, meta.run_started_at, now, threshold)?;
        }
        Ok(())
    }

    fn maybe_publish_stall(
        &self,
        id: &str,
        activity: &crate::store::ActivityTelemetry,
        run_started_at: chrono::DateTime<Utc>,
        now: chrono::DateTime<Utc>,
        threshold: u64,
    ) -> Result<()> {
        let baseline = activity.last_progress_at.unwrap_or(run_started_at);
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
        let request = activity
            .provider_request_id
            .as_deref()
            .map(|value| format!(", provider request {value}"))
            .unwrap_or_default();
        self.store.append_notification(
            id,
            "possible_stall",
            3,
            AgentStatus::Working,
            format!(
                "Possible stall: phase {}, inactive for {idle}s, retry count {}{request}",
                activity.current_phase.as_str(),
                activity.retry_count,
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
            final_answer: None,
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
        self.store.append_notification_payload(
            &id,
            "spawned",
            1,
            AgentStatus::Working,
            "Agent spawned",
            Some(json!({"task":message,"run_number":1})),
        )?;
        self.start_worker(meta.clone(), context)?;
        Ok(meta)
    }

    async fn enqueue_agent_input(
        &self,
        id: &str,
        message: String,
        wall_time_minutes: Option<u64>,
        wake: bool,
        receipt_type: &str,
        intent: &str,
    ) -> Result<Value> {
        let owner_lock = self.owner_lock(id);
        let _owner = owner_lock.lock().await;
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
            let deadline = Utc::now() + ChronoDuration::minutes(minutes as i64);
            if let Some(control) = self.active.lock().unwrap().get(id).cloned() {
                *control.deadline.lock().unwrap() = Some(deadline);
            }
            self.store.mutate_metadata(id, |meta| {
                if meta.status != AgentStatus::Working {
                    return Err(agent_state_conflict(id, meta.status.as_str()));
                }
                meta.deadline_at = Some(deadline);
                meta.updated_at = Utc::now();
                Ok(())
            })?;
            self.store.append_event(
                id,
                "lifecycle",
                json!({"status":"working","reason":"deadline_updated","deadline_at":deadline}),
            )?;
        }
        let message = self.store.enqueue_message(id, message, intent)?;
        self.store.append_notification_payload(
            id,
            intent,
            2,
            current.status.clone(),
            if intent == "followup" {
                "Follow-up accepted"
            } else {
                "Message accepted"
            },
            Some(json!({
                "message_id":message.id,
                "message_ref":message.local_ref,
                "intent":intent,
            })),
        )?;
        let (current_after_send, resume_state, agent_resumed) =
            if current.status == AgentStatus::Working {
                if let Some(control) = self.active.lock().unwrap().get(id).cloned() {
                    let _ = control.messages.send(());
                }
                (self.store.load_metadata(id)?, "not_needed", false)
            } else if !wake {
                (current, "not_woken", false)
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
            receipt_type,
        ))
    }

    pub async fn send(
        &self,
        id: &str,
        message: String,
        wall_time_minutes: Option<u64>,
    ) -> Result<Value> {
        self.enqueue_agent_input(
            id,
            message,
            wall_time_minutes,
            true,
            "message_sent",
            "followup",
        )
        .await
    }

    pub async fn followup(
        &self,
        id: &str,
        message: String,
        wall_time_minutes: Option<u64>,
    ) -> Result<Value> {
        self.enqueue_agent_input(
            id,
            message,
            wall_time_minutes,
            true,
            "followup_sent",
            "followup",
        )
        .await
    }

    pub async fn message(&self, id: &str, message: String) -> Result<Value> {
        self.enqueue_agent_input(id, message, None, false, "message_sent", "message")
            .await
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
        meta.final_answer = None;
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

    pub async fn rename(&self, id: &str, name: String) -> Result<Value> {
        let owner_lock = self.owner_lock(id);
        let _owner = owner_lock.lock().await;
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
        let local_ref = self.store.mutate_metadata(id, |meta| {
            meta.name = name.clone();
            Ok(meta.local_ref.clone())
        })?;
        Ok(json!({
            "type":"agent_renamed",
            "id":id,
            "ref":local_ref,
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

    pub fn team_values(&self, active_only: bool) -> Result<Vec<Value>> {
        let filter = crate::ipc::ListFilter {
            limit: usize::MAX,
            sort: "spawned_at".into(),
            order: "asc".into(),
            ..Default::default()
        };
        let mut values = Vec::new();
        let now = Utc::now();
        for agent in self.store.list(&filter)? {
            let pending = self.store.pending_messages(&agent.id)?;
            let waiting_for_capacity = agent.status != AgentStatus::Working
                && pending.iter().any(|message| message.intent == "followup");
            let sides = self.store.list_sides(&agent.id)?;
            let has_active_side = sides
                .iter()
                .any(|side| matches!(side.status, AgentStatus::Working | AgentStatus::Interrupted));
            if active_only
                && !matches!(
                    agent.status,
                    AgentStatus::Working | AgentStatus::Interrupted
                )
                && !waiting_for_capacity
                && !has_active_side
            {
                continue;
            }
            let agent_coordination_state = coordination_state(&agent.status, waiting_for_capacity);
            let latest_progress = self
                .store
                .latest_notification_for_owner(&agent.id)?
                .map(|notification| notification.summary);
            let context = self.store.load_context(&agent.id)?;
            let task = context.messages.iter().find_map(|message| {
                (message.get("role").and_then(Value::as_str) == Some("user"))
                    .then(|| message.get("content").and_then(Value::as_str))
                    .flatten()
            });
            values.push(json!({
                "type":"team_member",
                "resource":"agent",
                "id":agent.id,
                "ref":agent.local_ref,
                "name":agent.name,
                "task":task,
                "model":agent.model,
                "status":agent.status,
                "coordination_state":agent_coordination_state,
                "elapsed_seconds":elapsed_seconds(
                    agent.run_started_at,
                    agent_terminal_time(&agent).unwrap_or(now),
                ),
                "latest_progress":latest_progress,
                "pending_messages":pending.len(),
                "current_phase":agent.activity.current_phase,
                "run_number":agent.run_number,
                "final_answer":agent.final_answer,
            }));
            for side in sides {
                if active_only
                    && !matches!(side.status, AgentStatus::Working | AgentStatus::Interrupted)
                {
                    continue;
                }
                let latest_progress = self
                    .store
                    .latest_notification_for_owner(&side.id)?
                    .map(|notification| notification.summary);
                let side_coordination_state = coordination_state(&side.status, false);
                values.push(json!({
                    "type":"team_member",
                    "resource":"side",
                    "id":side.id,
                    "ref":side.local_ref,
                    "name":format!("Side {}",side.local_ref),
                    "parent_agent_id":side.agent_id,
                    "parent_agent_ref":side.agent_ref,
                    "task":side.question,
                    "model":side.model,
                    "status":side.status,
                    "coordination_state":side_coordination_state,
                    "elapsed_seconds":elapsed_seconds(
                        side.run_started_at,
                        side_terminal_time(&side).unwrap_or(now),
                    ),
                    "latest_progress":latest_progress,
                    "pending_messages":0,
                    "current_phase":side.activity.current_phase,
                    "run_number":1,
                    "final_answer":side.final_answer,
                }));
            }
        }
        let working_agents = self.working_count();
        values.push(json!({
            "type":"team_summary",
            "working_agents":working_agents,
            "max_agents":self.cfg.file.max_agents,
            "available_agent_slots":if self.cfg.file.max_agents==0 { Value::Null } else { json!(self.cfg.file.max_agents.saturating_sub(working_agents)) },
            "active_sides":self.active_sides.lock().unwrap().len(),
            "member_count":values.len(),
        }));
        Ok(values)
    }

    pub async fn schedule_pending(&self) -> Result<()> {
        if !self.accepting_work() {
            return Ok(());
        }
        let filter = crate::ipc::ListFilter {
            limit: usize::MAX,
            sort: "spawned_at".into(),
            order: "asc".into(),
            ..Default::default()
        };
        for meta in self.store.list(&filter)? {
            let owner_lock = self.owner_lock(&meta.id);
            let _owner = owner_lock.lock().await;
            let _operation = self.operations.lock().unwrap();
            if !self.accepting_work() {
                break;
            }
            if !self.has_capacity() {
                break;
            }
            if meta.status != AgentStatus::Working
                && self
                    .store
                    .pending_messages(&meta.id)?
                    .iter()
                    .any(|message| message.intent == "followup")
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
        let owner_lock = self.owner_lock(&id);
        tokio::spawn(async move {
            let mut outcome = run_worker(
                manager.cfg.clone(),
                manager.store.clone(),
                meta,
                context,
                message_rx,
                stop_rx,
                deadline,
                terminals,
                manager.operations.clone(),
                owner_lock.clone(),
            )
            .await;
            if let Err(error) = cleanup_terminals.terminate_all().await
                && outcome.is_ok()
            {
                outcome = Err(error);
            }
            {
                let mut active = manager.active.lock().unwrap();
                if active
                    .get(&id)
                    .is_some_and(|control| control.run_number == run_number)
                {
                    active.remove(&id);
                }
            }
            if let Err(e) = outcome {
                let _owner = owner_lock.lock().await;
                let _ = mark_failed(&manager.store, &id, format!("{e:#}"));
            }
            completed_tx.send(true).ok();
            if manager.accepting_work() {
                let _ = manager.schedule_pending().await;
            }
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
            control
        };
        let manager = self.clone();
        let id = id.to_string();
        let wait_id = id.clone();
        let reason = reason.to_string();
        let (result_tx, result_rx) = oneshot::channel();
        let finalizer_permit = self.track_internal_mutation();
        tokio::spawn(async move {
            let _finalizer_permit = finalizer_permit;
            let result = async {
                let owner_lock = manager.owner_lock(&id);
                let owner = owner_lock.lock().await;
                let current = manager.store.load_metadata(&id)?;
                if current.status != AgentStatus::Working {
                    return Err(agent_state_conflict(&id, current.status.as_str()));
                }
                manager.store.cancel_pending_messages(&id)?;
                control.stop.send(true).ok();
                control.terminals.terminate_all().await?;
                if !mark_stopped(&manager.store, &id, &reason)? {
                    let current = manager.store.load_metadata(&id)?;
                    return Err(agent_state_conflict(&id, current.status.as_str()));
                }
                let result = manager.store.load_metadata(&id)?;
                drop(owner);
                let mut completed = control.completed.clone();
                if !*completed.borrow() {
                    completed.changed().await.map_err(|_| {
                        coded_error(
                            "internal_error",
                            "agent worker ended without completing stop cleanup",
                            json!({"agent_id":id}),
                            true,
                        )
                    })?;
                }
                Ok(result)
            }
            .await;
            manager.stopping_agents.lock().unwrap().remove(&id);
            let _ = result_tx.send(result);
        });
        result_rx.await.map_err(|_| {
            coded_error(
                "internal_error",
                "agent stop finalizer ended without a result",
                json!({"agent_id":wait_id}),
                true,
            )
        })?
    }

    pub async fn interrupt(&self, id: &str) -> Result<AgentMetadata> {
        let control = {
            if self.stopping_agents.lock().unwrap().contains(id) {
                return Err(coded_error(
                    "conflict",
                    "agent is already stopping or interrupting",
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
                .ok_or_else(|| agent_state_conflict(id, "not working"))?;
            self.stopping_agents.lock().unwrap().insert(id.to_string());
            control
        };
        let manager = self.clone();
        let id = id.to_string();
        let wait_id = id.clone();
        let (result_tx, result_rx) = oneshot::channel();
        let finalizer_permit = self.track_internal_mutation();
        tokio::spawn(async move {
            let _permit = finalizer_permit;
            let result = async {
                let owner_lock = manager.owner_lock(&id);
                let owner = owner_lock.lock().await;
                let current = manager.store.load_metadata(&id)?;
                if current.status != AgentStatus::Working {
                    return Err(agent_state_conflict(&id, current.status.as_str()));
                }
                control.stop.send(true).ok();
                control.terminals.terminate_all().await?;
                if !mark_interrupted(&manager.store, &id, "user_request")? {
                    let current = manager.store.load_metadata(&id)?;
                    return Err(agent_state_conflict(&id, current.status.as_str()));
                }
                let result = manager.store.load_metadata(&id)?;
                drop(owner);
                let mut completed = control.completed.clone();
                if !*completed.borrow() {
                    completed.changed().await.map_err(|_| {
                        coded_error(
                            "internal_error",
                            "agent worker ended without completing interrupt cleanup",
                            json!({"agent_id":id}),
                            true,
                        )
                    })?;
                }
                Ok(result)
            }
            .await;
            manager.stopping_agents.lock().unwrap().remove(&id);
            let _ = result_tx.send(result);
        });
        result_rx.await.map_err(|_| {
            coded_error(
                "internal_error",
                "agent interrupt finalizer ended without a result",
                json!({"agent_id":wait_id}),
                true,
            )
        })?
    }

    pub async fn cancel_message(&self, id: &str, message_id: &str) -> Result<MessageRecord> {
        let owner_lock = self.owner_lock(id);
        let _owner = owner_lock.lock().await;
        let _operation = self.operations.lock().unwrap();
        self.store.cancel_message(id, message_id)
    }

    pub async fn update_time(&self, id: &str, minutes: u64) -> Result<AgentMetadata> {
        if !(1..=6000).contains(&minutes) {
            return Err(coded_error(
                "invalid_argument",
                "minutes must be from 1 through 6000",
                json!({"field":"minutes"}),
                false,
            ));
        }
        let owner_lock = self.owner_lock(id);
        let _owner = owner_lock.lock().await;
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
        let meta = self.store.mutate_metadata(id, |meta| {
            if meta.status != AgentStatus::Working {
                return Err(agent_state_conflict(id, meta.status.as_str()));
            }
            meta.deadline_at = Some(deadline);
            meta.updated_at = Utc::now();
            Ok(meta.clone())
        })?;
        self.store.append_event(
            id,
            "lifecycle",
            json!({"status":"working","reason":"deadline_updated","deadline_at":deadline}),
        )?;
        Ok(meta)
    }

    pub async fn create_side(
        &self,
        id: &str,
        message: String,
        model: Option<String>,
        wall_time_minutes: Option<u64>,
    ) -> Result<Value> {
        let owner_lock = self.owner_lock(id);
        let _owner = owner_lock.lock().await;
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
        let inherited_context_messages = context.messages.len();
        let side_system_prompt =
            prompts::render(prompts::SIDE, &[("working_directory", meta.dir.as_str())])?;
        context.messages.push(json!({
            "role":"system",
            "content":&side_system_prompt
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
            final_answer: None,
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
        self.store.append_notification_payload(
            &side_id,
            "spawned",
            1,
            AgentStatus::Working,
            "Side agent spawned",
            Some(json!({"question":message,"parent_agent_id":id,"parent_agent_ref":agent_ref})),
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
        let side_owner_lock = self.owner_lock(&side_id);
        tokio::spawn(async move {
            let mut result = run_side(
                manager.cfg.clone(),
                manager.store.clone(),
                meta,
                side,
                context,
                stop_rx,
                terminals.clone(),
                side_owner_lock.clone(),
            )
            .await;
            if let Err(error) = terminals.terminate_all().await
                && result.is_ok()
            {
                result = Err(error);
            }
            manager.active_sides.lock().unwrap().remove(&run_id);
            if let Err(error) = result {
                let _owner = side_owner_lock.lock().await;
                let _ = mark_side_failed(&manager.store, &run_id, format!("{error:#}"));
            }
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
        let control = {
            if self.stopping_sides.lock().unwrap().contains(id) {
                return Err(coded_error(
                    "conflict",
                    "side is already stopping",
                    json!({"side_id":id,"status":"stopping"}),
                    true,
                ));
            }
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
            self.stopping_sides.lock().unwrap().insert(id.to_string());
            control
        };
        let manager = self.clone();
        let id = id.to_string();
        let wait_id = id.clone();
        let reason = reason.to_string();
        let (result_tx, result_rx) = oneshot::channel();
        let finalizer_permit = self.track_internal_mutation();
        tokio::spawn(async move {
            let _finalizer_permit = finalizer_permit;
            let result = async {
                let owner_lock = manager.owner_lock(&id);
                let owner = owner_lock.lock().await;
                let current = manager.store.load_side_metadata(&id)?;
                if current.status != AgentStatus::Working {
                    return Err(side_state_conflict(&id, current.status.as_str()));
                }
                control.stop.send(true).ok();
                control.terminals.terminate_all().await?;
                if !mark_side_stopped(&manager.store, &id, &reason)? {
                    let current = manager.store.load_side_metadata(&id)?;
                    return Err(side_state_conflict(&id, current.status.as_str()));
                }
                let result = manager.store.load_side_metadata(&id)?;
                drop(owner);
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
                Ok(result)
            }
            .await;
            manager.stopping_sides.lock().unwrap().remove(&id);
            let _ = result_tx.send(result);
        });
        result_rx.await.map_err(|_| {
            coded_error(
                "internal_error",
                "Side stop finalizer ended without a result",
                json!({"side_id":wait_id}),
                true,
            )
        })?
    }

    pub async fn delete_agent(&self, id: &str) -> Result<Value> {
        let manager = self.clone();
        let id = id.to_string();
        let wait_id = id.clone();
        let (result_tx, result_rx) = oneshot::channel();
        let finalizer_permit = self.track_internal_mutation();
        tokio::spawn(async move {
            let _finalizer_permit = finalizer_permit;
            let result = manager.delete_agent_finalizer(&id).await;
            let _ = result_tx.send(result);
        });
        result_rx.await.map_err(|_| {
            coded_error(
                "internal_error",
                "agent delete finalizer ended without a result",
                json!({"agent_id":wait_id}),
                true,
            )
        })?
    }

    async fn delete_agent_finalizer(&self, id: &str) -> Result<Value> {
        let owner_lock = self.owner_lock(id);
        let _owner = owner_lock.lock().await;
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

    pub async fn delete_side(&self, id: &str) -> Result<Value> {
        let owner_lock = self.owner_lock(id);
        let _owner = owner_lock.lock().await;
        let side = self.store.load_side_metadata(id)?;
        if side.status == AgentStatus::Working {
            return Err(side_state_conflict(id, side.status.as_str()));
        }
        self.store.delete_side(id)?;
        Ok(
            json!({"type":"side_deleted","id":id,"ref":side.local_ref,"agent_id":side.agent_id,"agent_ref":side.agent_ref}),
        )
    }

    pub async fn stop_all(&self, reason: &str) -> Result<()> {
        let mut errors = Vec::new();
        loop {
            let ids = self
                .active
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            let side_ids = self
                .active_sides
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            if ids.is_empty() && side_ids.is_empty() {
                break;
            }
            for id in ids {
                if let Err(error) = self.stop(&id, reason).await {
                    let naturally_terminal = self
                        .store
                        .load_metadata(&id)
                        .is_ok_and(|meta| meta.status != AgentStatus::Working);
                    if !naturally_terminal {
                        errors.push(format!("Agent {id}: {error:#}"));
                    }
                }
            }
            for id in side_ids {
                if let Err(error) = self.stop_side(&id, reason).await {
                    let naturally_terminal = self
                        .store
                        .load_side_metadata(&id)
                        .is_ok_and(|meta| meta.status != AgentStatus::Working);
                    if !naturally_terminal {
                        errors.push(format!("Side {id}: {error:#}"));
                    }
                }
            }
            if !errors.is_empty() {
                break;
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(coded_error(
                "shutdown_failed",
                "one or more workers could not be stopped",
                json!({"errors":errors}),
                true,
            ))
        }
    }
}

fn coordination_state(status: &AgentStatus, waiting_for_capacity: bool) -> &'static str {
    if waiting_for_capacity {
        return "waiting_for_capacity";
    }
    match status {
        AgentStatus::Working => "busy",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Finished => "completed",
        AgentStatus::Stopped => "stopped",
        AgentStatus::Failed => "failed",
    }
}

fn elapsed_seconds(started_at: chrono::DateTime<Utc>, ended_at: chrono::DateTime<Utc>) -> i64 {
    (ended_at - started_at).num_seconds().max(0)
}

fn agent_terminal_time(meta: &AgentMetadata) -> Option<chrono::DateTime<Utc>> {
    match meta.status {
        AgentStatus::Working => None,
        AgentStatus::Interrupted => meta.activity.last_state_change_at,
        AgentStatus::Finished => meta.finished_at,
        AgentStatus::Stopped => meta.stopped_at,
        AgentStatus::Failed => meta.failed_at,
    }
}

fn side_terminal_time(meta: &SideMetadata) -> Option<chrono::DateTime<Utc>> {
    match meta.status {
        AgentStatus::Working => None,
        AgentStatus::Interrupted => meta.activity.last_state_change_at,
        AgentStatus::Finished => meta.finished_at,
        AgentStatus::Stopped => meta.stopped_at,
        AgentStatus::Failed => meta.failed_at,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_side(
    cfg: Arc<RuntimeConfig>,
    store: Store,
    parent: AgentMetadata,
    side: SideMetadata,
    mut context: ContextSnapshot,
    mut stop: watch::Receiver<bool>,
    terminals: TerminalManager,
    owner_lock: Arc<AsyncMutex<()>>,
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
    let mut retried_empty_completion = false;
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        if context_needs_compaction(&context, cfg.file.context_token_budget) {
            begin_side_model_request(&store, &side.id)?;
            let mut last_activity_write = None;
            let compaction = compact_context(
                &client,
                &mut context,
                cfg.file.context_token_budget,
                |progress| {
                    record_side_model_progress(&store, &side.id, progress, &mut last_activity_write)
                },
            );
            let changed = if let Some(deadline) = side.deadline_at {
                let remaining = (deadline - Utc::now())
                    .to_std()
                    .unwrap_or(std::time::Duration::ZERO);
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    result = tokio::time::timeout(remaining, compaction) => match result {
                        Ok(value) => value?,
                        Err(_) => {
                            let _owner = owner_lock.lock().await;
                            mark_side_stopped(&store, &side.id, "wall_time")?;
                            return Ok(());
                        }
                    }
                }
            } else {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    result = compaction => result?,
                }
            };
            complete_side_model_request(&store, &side.id)?;
            if changed {
                store.save_side_context(&side.id, &context)?;
            }
        }
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
                    Err(_) => {
                        let _owner = owner_lock.lock().await;
                        mark_side_stopped(&store, &side.id, "wall_time")?;
                        return Ok(());
                    }
                }
            }
        } else {
            tokio::select! {
                _ = stop.changed() => return Ok(()),
                result = completion => result?,
            }
        };
        if turn.tool_calls.is_empty() && turn.content.trim().is_empty() {
            if retried_empty_completion {
                return Err(coded_error(
                    "empty_completion",
                    "model returned two consecutive empty completions",
                    json!({"side_id":side.id,"attempts":2}),
                    false,
                ));
            }
            retried_empty_completion = true;
            context.messages.push(json!({
                "role":"system",
                "content":prompts::render(prompts::EMPTY_COMPLETION_RETRY, &[])?
            }));
            store.save_side_context(&side.id, &context)?;
            continue;
        }
        retried_empty_completion = false;
        complete_side_model_request(&store, &side.id)?;
        if !turn.reasoning.is_empty() {
            store.append_side_event(&side.id, "reasoning", json!({"content":turn.reasoning}))?;
        }
        context.messages.push(assistant_message(&turn));
        store.save_side_context(&side.id, &context)?;
        if turn.tool_calls.is_empty() {
            if turn.content.len() > 1024 * 1024 {
                return Err(coded_error(
                    "final_answer_too_large",
                    "Side final answer exceeds 1048576 UTF-8 bytes",
                    json!({"side_id":side.id,"max_bytes":1048576}),
                    false,
                ));
            }
            let answer_event = store.append_side_event(
                &side.id,
                "assistant_message",
                json!({"content":turn.content,"usage":turn.usage}),
            )?;
            let _owner = owner_lock.lock().await;
            mark_side_finished(&store, &side.id, turn.content, &answer_event)?;
            return Ok(());
        }
        let mut image_messages = Vec::new();
        for call in turn.tool_calls {
            let phase = if call.function.name == "write_stdin" {
                crate::store::AgentPhase::WaitingTerminal
            } else {
                crate::store::AgentPhase::ExecutingTool
            };
            {
                let _owner = owner_lock.lock().await;
                set_side_phase(&store, &side.id, phase)?;
                store.mutate_side_metadata(&side.id, |current| {
                    if current.status != AgentStatus::Working {
                        return Err(side_state_conflict(&side.id, current.status.as_str()));
                    }
                    current.tool_calls = current.tool_calls.saturating_add(1);
                    Ok(())
                })?;
                store.append_side_event(&side.id, "tool_call", json!({"tool_call_id":call.id,"name":call.function.name,"arguments":call.function.arguments}))?;
            }
            let result = tokio::select! {
                result = runtime.execute(&call) => result,
                changed = stop.changed() => {
                    if changed.is_ok() && *stop.borrow() {
                        runtime.terminals.terminate_all().await?;
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
    owner_lock: Arc<AsyncMutex<()>>,
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
    let mut retried_empty_completion = false;
    loop {
        while message_rx.try_recv().is_ok() {}
        {
            let _operation = operations.lock().unwrap();
            deliver_pending_messages(&store, &meta.id, &mut context)?;
        }
        if context_needs_compaction(&context, cfg.file.context_token_budget) {
            begin_agent_model_request(&store, &meta.id)?;
            let mut last_activity_write = None;
            let changed = {
                let compaction = compact_context(
                    &client,
                    &mut context,
                    cfg.file.context_token_budget,
                    |progress| {
                        record_agent_model_progress(
                            &store,
                            &meta.id,
                            progress,
                            &mut last_activity_write,
                        )
                    },
                );
                tokio::pin!(compaction);
                loop {
                    tokio::select! {
                        result = &mut compaction => break result?,
                        changed = stop_rx.changed() => {
                            if changed.is_ok() && *stop_rx.borrow() {
                                terminals.terminate_all().await?;
                                return Ok(());
                            }
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                            if deadline.lock().unwrap().is_some_and(|d| Utc::now() >= d) {
                                terminals.terminate_all().await?;
                                let _owner = owner_lock.lock().await;
                                mark_stopped(&store, &meta.id, "wall_time")?;
                                return Ok(());
                            }
                        }
                    }
                }
            };
            complete_agent_model_request(&store, &meta.id)?;
            if changed {
                store.save_context(&meta.id, &context)?;
            }
        }
        if *stop_rx.borrow() {
            terminals.terminate_all().await?;
            return Ok(());
        }
        if deadline.lock().unwrap().is_some_and(|d| Utc::now() >= d) {
            terminals.terminate_all().await?;
            let _owner = owner_lock.lock().await;
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
                changed=stop_rx.changed()=>{if changed.is_ok()&&*stop_rx.borrow(){terminals.terminate_all().await?;return Ok(())}},
                _=tokio::time::sleep(std::time::Duration::from_secs(1))=>{if deadline.lock().unwrap().is_some_and(|d|Utc::now()>=d){terminals.terminate_all().await?;let _owner=owner_lock.lock().await;mark_stopped(&store,&meta.id,"wall_time")?;return Ok(())}}
            }
        };
        if turn.tool_calls.is_empty() && turn.content.trim().is_empty() {
            if retried_empty_completion {
                return Err(coded_error(
                    "empty_completion",
                    "model returned two consecutive empty completions",
                    json!({"agent_id":meta.id,"run_number":meta.run_number,"attempts":2}),
                    false,
                ));
            }
            retried_empty_completion = true;
            context.messages.push(json!({
                "role":"system",
                "content":prompts::render(prompts::EMPTY_COMPLETION_RETRY, &[])?
            }));
            store.save_context(&meta.id, &context)?;
            continue;
        }
        retried_empty_completion = false;
        complete_agent_model_request(&store, &meta.id)?;
        if !turn.reasoning.is_empty() {
            store.append_event(&meta.id, "reasoning", json!({"content":turn.reasoning}))?;
        }
        let assistant_event = if !turn.content.is_empty() {
            Some(store.append_event(
                &meta.id,
                "assistant_message",
                json!({"content":turn.content,"usage":turn.usage}),
            )?)
        } else {
            None
        };
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
            terminals.terminate_all().await?;
            let _owner = owner_lock.lock().await;
            if turn.content.len() > 1024 * 1024 {
                return Err(coded_error(
                    "final_answer_too_large",
                    "Agent final answer exceeds 1048576 UTF-8 bytes",
                    json!({"agent_id":meta.id,"run_number":meta.run_number,"max_bytes":1048576}),
                    false,
                ));
            }
            mark_finished(
                &store,
                &meta.id,
                &turn.content,
                assistant_event
                    .as_ref()
                    .expect("nonempty completion has an Event"),
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
            set_agent_phase(&store, &meta.id, phase)?;
            store.append_event(&meta.id,"tool_call",json!({"tool_call_id":call.id,"name":call.function.name,"arguments":call.function.arguments}))?;
            let result = tokio::select! {
                result = runtime.execute(&call) => result,
                changed = stop_rx.changed() => {
                    if changed.is_ok() && *stop_rx.borrow() {
                        terminals.terminate_all().await?;
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
    receipt_type: &str,
) -> Value {
    json!({
        "type":receipt_type,
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
        "intent":message.intent,
    })
}

fn system_message(meta: &AgentMetadata) -> Value {
    let mode_template = match meta.mode {
        AgentMode::Readonly => prompts::AGENT_READONLY,
        AgentMode::Write => prompts::AGENT_WRITE,
    };
    let mode = prompts::render(mode_template, &[]).expect("bundled mode prompt must render");
    let content = prompts::render(
        prompts::AGENT,
        &[
            ("working_directory", meta.dir.as_str()),
            ("mode_instructions", mode.as_str()),
        ],
    )
    .expect("bundled Agent prompt must render");
    json!({"role":"system","content":content})
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

fn context_needs_compaction(context: &ContextSnapshot, budget: usize) -> bool {
    estimated_tokens(&context.messages) > budget
}

async fn compact_context(
    client: &OpenAiClient,
    context: &mut ContextSnapshot,
    budget: usize,
    mut progress: impl FnMut(ModelProgress) -> Result<()>,
) -> Result<bool> {
    if !context_needs_compaction(context, budget) {
        return Ok(false);
    }
    let Some(cut) = compaction_boundary(&context.messages) else {
        return Err(coded_error(
            "context_compaction_failed",
            "context exceeds its budget but cannot be split at a safe turn boundary",
            json!({"estimated_tokens":estimated_tokens(&context.messages),"budget":budget}),
            false,
        ));
    };
    let earlier = context.messages[1..cut].to_vec();
    let retained = context.messages[cut..].to_vec();
    let target_tokens = (budget / 10).clamp(128, 4_000);
    let target_words = (target_tokens * 3 / 4).max(96).to_string();
    let request_prompt = prompts::render(
        prompts::CONTEXT_SUMMARY_REQUEST,
        &[("target_words", target_words.as_str())],
    )?;
    let mut summary_messages = Vec::with_capacity(earlier.len() + 2);
    summary_messages.push(json!({"role":"system","content":request_prompt}));
    summary_messages.extend(earlier);
    summary_messages.push(json!({
        "role":"user",
        "content":"Produce the compacted handoff now. Output only the summary text."
    }));
    let turn = client
        .complete_text_observed(&summary_messages, &mut progress)
        .await?;
    let mut summary = turn.content.trim().to_string();
    if summary.is_empty() {
        return Err(coded_error(
            "context_compaction_failed",
            "the context summarization request returned empty content",
            json!({"estimated_tokens":estimated_tokens(&context.messages),"budget":budget}),
            true,
        ));
    }

    let mut replacement = compacted_messages(&context.messages[0], &summary, &retained)?;
    if estimated_tokens(&replacement) > budget || summary.len() / 4 > target_tokens {
        let tighter_words = (target_words.parse::<usize>().unwrap_or(96) / 2)
            .max(64)
            .to_string();
        let tighter_prompt = prompts::render(
            prompts::CONTEXT_SUMMARY_REQUEST,
            &[("target_words", tighter_words.as_str())],
        )?;
        let compression_request = vec![
            json!({"role":"system","content":tighter_prompt}),
            json!({"role":"user","content":summary}),
            json!({"role":"user","content":"Compress this handoff further. Output only the summary text."}),
        ];
        let turn = client
            .complete_text_observed(&compression_request, &mut progress)
            .await?;
        summary = turn.content.trim().to_string();
        if summary.is_empty() {
            return Err(coded_error(
                "context_compaction_failed",
                "the tighter context summarization request returned empty content",
                json!({"budget":budget}),
                true,
            ));
        }
        replacement = compacted_messages(&context.messages[0], &summary, &retained)?;
    }
    if estimated_tokens(&replacement) > budget {
        return Err(coded_error(
            "context_compaction_failed",
            "the model-generated summary and retained context still exceed the context budget",
            json!({"estimated_tokens":estimated_tokens(&replacement),"budget":budget}),
            true,
        ));
    }
    context.messages = replacement;
    context.compacted_at = Some(Utc::now());
    Ok(true)
}

fn compacted_messages(system: &Value, summary: &str, retained: &[Value]) -> Result<Vec<Value>> {
    let wrapper = prompts::render(prompts::CONTEXT_COMPACTION, &[("summary", summary)])?;
    let mut messages = Vec::with_capacity(retained.len() + 2);
    messages.push(system.clone());
    messages.push(json!({"role":"system","content":wrapper}));
    messages.extend_from_slice(retained);
    Ok(messages)
}

fn compaction_boundary(messages: &[Value]) -> Option<usize> {
    if messages.len() < 3 {
        return None;
    }
    let weights = messages[1..].iter().map(message_weight).collect::<Vec<_>>();
    let target = weights.iter().sum::<usize>() * 3 / 5;
    let mut cumulative = 0usize;
    let mut best = None;
    for cut in 2..messages.len() {
        cumulative = cumulative.saturating_add(weights[cut - 2]);
        if !safe_compaction_boundary(messages, cut) {
            continue;
        }
        let distance = cumulative.abs_diff(target);
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((cut, distance));
        }
    }
    best.map(|(cut, _)| cut)
}

fn safe_compaction_boundary(messages: &[Value], cut: usize) -> bool {
    cut > 1
        && cut < messages.len()
        && messages[cut].get("role").and_then(Value::as_str) != Some("tool")
}

fn message_weight(message: &Value) -> usize {
    serde_json::to_vec(message).map_or(1, |bytes| bytes.len().max(1))
}

fn estimated_tokens(messages: &[Value]) -> usize {
    serde_json::to_vec(messages)
        .map(|b| b.len() / 4)
        .unwrap_or(0)
}

fn mark_finished(
    store: &Store,
    id: &str,
    final_message: &str,
    answer_event: &crate::store::EventRecord,
) -> Result<bool> {
    let created_at = answer_event.timestamp;
    let event_id = answer_event.event_id.clone();
    let event_ref = answer_event.local_ref.clone();
    let mut answer = None;
    let transitioned = store.mutate_metadata(id, |m| {
        if m.status != AgentStatus::Working {
            return Ok(false);
        }
        let now = Utc::now();
        let final_answer = crate::store::FinalAnswer {
            run_number: m.run_number,
            content: final_message.to_string(),
            event_id: event_id.clone(),
            event_ref: event_ref.clone(),
            created_at,
        };
        m.final_answer = Some(final_answer.clone());
        answer = Some(final_answer);
        set_agent_terminal(m, AgentStatus::Finished, now);
        Ok(true)
    })?;
    if !transitioned {
        return Ok(false);
    }
    store.append_event(id, "lifecycle", json!({"status":"finished"}))?;
    store.append_notification_payload(
        id,
        "finished",
        2,
        AgentStatus::Finished,
        final_message,
        Some(json!({"final_answer":answer.expect("transitioned answer")})),
    )?;
    Ok(true)
}
fn mark_stopped(store: &Store, id: &str, reason: &str) -> Result<bool> {
    let transitioned = store.mutate_metadata(id, |m| {
        if m.status != AgentStatus::Working {
            return Ok(false);
        }
        let now = Utc::now();
        set_agent_terminal(m, AgentStatus::Stopped, now);
        m.stop_reason = Some(reason.into());
        Ok(true)
    })?;
    if !transitioned {
        return Ok(false);
    }
    store.append_event(id, "lifecycle", json!({"status":"stopped","reason":reason}))?;
    store.append_notification(
        id,
        "stopped",
        3,
        AgentStatus::Stopped,
        format!("Agent stopped: {reason}"),
    )?;
    Ok(true)
}
fn mark_interrupted(store: &Store, id: &str, reason: &str) -> Result<bool> {
    let transitioned = store.mutate_metadata(id, |meta| {
        if meta.status != AgentStatus::Working {
            return Ok(false);
        }
        let now = Utc::now();
        meta.status = AgentStatus::Interrupted;
        meta.updated_at = now;
        meta.finished_at = None;
        meta.stopped_at = None;
        meta.failed_at = None;
        meta.deadline_at = None;
        meta.stop_reason = Some(reason.into());
        meta.last_error = None;
        meta.final_answer = None;
        meta.activity.current_phase = crate::store::AgentPhase::Interrupted;
        meta.activity.phase_started_at = Some(now);
        meta.activity.last_state_change_at = Some(now);
        clear_active_request(&mut meta.activity);
        Ok(true)
    })?;
    if !transitioned {
        return Ok(false);
    }
    store.append_event(
        id,
        "lifecycle",
        json!({"status":"interrupted","reason":reason}),
    )?;
    store.append_notification_payload(
        id,
        "interrupted",
        3,
        AgentStatus::Interrupted,
        "Agent interrupted; follow up to resume",
        Some(json!({"reason":reason})),
    )?;
    Ok(true)
}
fn mark_failed(store: &Store, id: &str, error: String) -> Result<bool> {
    let transitioned = store.mutate_metadata(id, |m| {
        if m.status != AgentStatus::Working {
            return Ok(false);
        }
        let now = Utc::now();
        set_agent_terminal(m, AgentStatus::Failed, now);
        m.last_error = Some(error.clone());
        Ok(true)
    })?;
    if !transitioned {
        return Ok(false);
    }
    store.append_event(id, "error", json!({"status":"failed","error":error}))?;
    store.append_notification(id, "failed", 4, AgentStatus::Failed, &error)?;
    Ok(true)
}

fn mark_side_finished(
    store: &Store,
    id: &str,
    answer: String,
    answer_event: &crate::store::EventRecord,
) -> Result<bool> {
    let summary = answer.clone();
    let mut final_answer = None;
    let transitioned = store.mutate_side_metadata(id, |meta| {
        if meta.status != AgentStatus::Working {
            return Ok(false);
        }
        let now = Utc::now();
        set_side_terminal(meta, AgentStatus::Finished, now);
        meta.answer = Some(answer.clone());
        let value = crate::store::FinalAnswer {
            run_number: 1,
            content: answer.clone(),
            event_id: answer_event.event_id.clone(),
            event_ref: answer_event.local_ref.clone(),
            created_at: answer_event.timestamp,
        };
        meta.final_answer = Some(value.clone());
        final_answer = Some(value);
        Ok(true)
    })?;
    if !transitioned {
        return Ok(false);
    }
    store.append_side_event(id, "lifecycle", json!({"status":"finished"}))?;
    store.append_notification_payload(
        id,
        "finished",
        2,
        AgentStatus::Finished,
        summary,
        Some(json!({"final_answer":final_answer.expect("transitioned Side answer")})),
    )?;
    Ok(true)
}

fn mark_side_stopped(store: &Store, id: &str, reason: &str) -> Result<bool> {
    let transitioned = store.mutate_side_metadata(id, |meta| {
        if meta.status != AgentStatus::Working {
            return Ok(false);
        }
        let now = Utc::now();
        set_side_terminal(meta, AgentStatus::Stopped, now);
        meta.stop_reason = Some(reason.into());
        Ok(true)
    })?;
    if !transitioned {
        return Ok(false);
    }
    store.append_side_event(id, "lifecycle", json!({"status":"stopped","reason":reason}))?;
    store.append_notification(
        id,
        "stopped",
        3,
        AgentStatus::Stopped,
        format!("Side agent stopped: {reason}"),
    )?;
    Ok(true)
}

fn mark_side_failed(store: &Store, id: &str, error: String) -> Result<bool> {
    let transitioned = store.mutate_side_metadata(id, |meta| {
        if meta.status != AgentStatus::Working {
            return Ok(false);
        }
        let now = Utc::now();
        set_side_terminal(meta, AgentStatus::Failed, now);
        meta.last_error = Some(error.clone());
        Ok(true)
    })?;
    if !transitioned {
        return Ok(false);
    }
    store.append_side_event(id, "error", json!({"status":"failed","error":error}))?;
    store.append_notification(id, "failed", 4, AgentStatus::Failed, &error)?;
    Ok(true)
}

fn set_agent_terminal(meta: &mut AgentMetadata, status: AgentStatus, now: chrono::DateTime<Utc>) {
    meta.status = status.clone();
    meta.updated_at = now;
    meta.finished_at = (status == AgentStatus::Finished).then_some(now);
    meta.stopped_at = (status == AgentStatus::Stopped).then_some(now);
    meta.failed_at = (status == AgentStatus::Failed).then_some(now);
    meta.deadline_at = None;
    meta.stop_reason = None;
    meta.last_error = None;
    meta.activity.current_phase = match status {
        AgentStatus::Finished => crate::store::AgentPhase::Finished,
        AgentStatus::Interrupted => crate::store::AgentPhase::Interrupted,
        AgentStatus::Stopped => crate::store::AgentPhase::Stopped,
        AgentStatus::Failed => crate::store::AgentPhase::Failed,
        AgentStatus::Working => unreachable!(),
    };
    meta.activity.phase_started_at = Some(now);
    meta.activity.last_state_change_at = Some(now);
    clear_active_request(&mut meta.activity);
}

fn set_side_terminal(meta: &mut SideMetadata, status: AgentStatus, now: chrono::DateTime<Utc>) {
    meta.status = status.clone();
    meta.updated_at = now;
    meta.finished_at = (status == AgentStatus::Finished).then_some(now);
    meta.stopped_at = (status == AgentStatus::Stopped).then_some(now);
    meta.failed_at = (status == AgentStatus::Failed).then_some(now);
    meta.deadline_at = None;
    meta.stop_reason = None;
    meta.last_error = None;
    meta.activity.current_phase = match status {
        AgentStatus::Finished => crate::store::AgentPhase::Finished,
        AgentStatus::Interrupted => crate::store::AgentPhase::Interrupted,
        AgentStatus::Stopped => crate::store::AgentPhase::Stopped,
        AgentStatus::Failed => crate::store::AgentPhase::Failed,
        AgentStatus::Working => unreachable!(),
    };
    meta.activity.phase_started_at = Some(now);
    meta.activity.last_state_change_at = Some(now);
    clear_active_request(&mut meta.activity);
}

fn clear_active_request(activity: &mut crate::store::ActivityTelemetry) {
    if activity.last_provider_request_id.is_none() {
        activity.last_provider_request_id = activity.provider_request_id.clone();
    }
    activity.request_started_at = None;
    activity.provider_request_id = None;
}

fn agent_state_conflict(id: &str, status: &str) -> anyhow::Error {
    coded_error(
        "conflict",
        format!("agent is not working: {status}"),
        json!({"agent_id":id,"status":status}),
        false,
    )
}

fn side_state_conflict(id: &str, status: &str) -> anyhow::Error {
    coded_error(
        "conflict",
        format!("side is not working: {status}"),
        json!({"side_id":id,"status":status}),
        false,
    )
}

fn daemon_stopping_error() -> anyhow::Error {
    coded_error(
        "conflict",
        "daemon is stopping and no longer accepts mutations",
        json!({"status":"stopping"}),
        true,
    )
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
    fn compaction_boundary_is_near_sixty_percent_and_keeps_tool_turns_whole() {
        let mut messages = vec![
            json!({"role":"system","content":"system"}),
            json!({"role":"user","content":"task"}),
        ];
        for index in 0..12 {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call_{index}"),"type":"function","function":{"name":"read","arguments":"{}"}}]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("call_{index}"),"content":"x".repeat(2000)}));
        }
        let cut = compaction_boundary(&messages).expect("context has safe boundaries");
        assert_ne!(
            messages[cut].get("role").and_then(Value::as_str),
            Some("tool")
        );
        assert!(cut > 2);
        assert!(cut < messages.len());
        let removed_weight = messages[1..cut].iter().map(message_weight).sum::<usize>();
        let total_weight = messages[1..].iter().map(message_weight).sum::<usize>();
        let removed_ratio = removed_weight as f64 / total_weight as f64;
        assert!(
            (0.50..=0.70).contains(&removed_ratio),
            "ratio={removed_ratio}"
        );
    }

    #[test]
    fn compaction_replacement_preserves_system_and_recent_messages_verbatim() {
        let system = json!({"role":"system","content":"immutable"});
        let retained = vec![
            json!({"role":"user","content":"recent question"}),
            json!({"role":"assistant","content":"recent answer"}),
        ];
        let replacement = compacted_messages(&system, "older facts", &retained).unwrap();
        assert_eq!(replacement[0], system);
        assert_eq!(&replacement[2..], retained.as_slice());
        assert!(
            replacement[1]["content"]
                .as_str()
                .unwrap()
                .contains("older facts")
        );
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

    #[tokio::test]
    async fn mutation_gate_cannot_lose_the_final_idle_notification() {
        let gate = Arc::new(MutationGate {
            accepting: AtomicBool::new(true),
            active: AtomicUsize::new(1),
            idle: Notify::new(),
        });
        let permit = MutationPermit { gate: gate.clone() };
        gate.accepting.store(false, Ordering::Release);
        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            loop {
                let notified = waiter_gate.idle.notified();
                if waiter_gate.active.load(Ordering::Acquire) == 0 {
                    return;
                }
                notified.await;
            }
        });
        drop(permit);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("idle waiter must not hang")
            .unwrap();
    }
}
