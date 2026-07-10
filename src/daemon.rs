use crate::{
    agent::AgentManager,
    config::{RuntimeConfig, ensure_private_dir},
    ipc::{Request, error_json},
    store::{EventRecord, Store, canonical_dir},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::watch,
};

pub async fn serve(cfg: RuntimeConfig) -> Result<()> {
    ensure_private_dir(&cfg.paths.runtime_dir)?;
    let lock_path = cfg.paths.daemon_lock();
    let _lock = acquire_daemon_lock(&lock_path)?;
    let socket = cfg.paths.socket();
    if socket.exists() {
        std::fs::remove_file(&socket).context("remove stale daemon socket")?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    set_socket_permissions(&socket)?;
    let store = Store::new(&cfg.paths)?;
    let recovered = store.recover_interrupted()?;
    let cfg = Arc::new(cfg);
    let manager = AgentManager::new(cfg.clone(), store.clone());
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    if recovered > 0 {
        eprintln!("recovered {recovered} interrupted agents as stopped");
    }
    loop {
        tokio::select! {
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;let manager=manager.clone();let store=store.clone();let cfg=cfg.clone();let tx=shutdown_tx.clone();
                tokio::spawn(async move{if let Err(e)=handle_connection(stream,manager,store,cfg,tx).await{eprintln!("ipc error: {e:#}");}});
            }
            _=shutdown_rx.changed()=>{if *shutdown_rx.borrow(){break}}
        }
    }
    manager.stop_all("daemon_shutdown").await;
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(lock_path);
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    manager: AgentManager,
    store: Store,
    cfg: Arc<RuntimeConfig>,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let line = lines.next_line().await?.context("empty request")?;
    let request: Request = serde_json::from_str(&line).context("invalid request JSON")?;
    match dispatch(request, &manager, &store, &cfg, &shutdown).await {
        Ok(Output::Lines(values)) => {
            for value in values {
                write_json_line(&mut write_half, &value).await?
            }
        }
        Ok(Output::Follow {
            id,
            types,
            after,
            limit,
        }) => {
            follow_logs(
                &mut write_half,
                &store,
                &id,
                &types,
                after.as_deref(),
                limit,
            )
            .await?
        }
        Err(e) => {
            write_json_line(
                &mut write_half,
                &error_json(error_code(&e), format!("{e:#}")),
            )
            .await?
        }
    }
    Ok(())
}

enum Output {
    Lines(Vec<Value>),
    Follow {
        id: String,
        types: Vec<String>,
        after: Option<String>,
        limit: usize,
    },
}

async fn dispatch(
    req: Request,
    manager: &AgentManager,
    store: &Store,
    cfg: &RuntimeConfig,
    shutdown: &watch::Sender<bool>,
) -> Result<Output> {
    let lines = match req {
        Request::DaemonStatus => vec![
            json!({"type":"daemon","status":"running","pid":std::process::id(),"socket":cfg.paths.socket(),"working_agents":manager.working_count(),"max_agents":cfg.file.max_agents,"model":cfg.file.model,"base_url":cfg.file.base_url}),
        ],
        Request::DaemonStop => {
            shutdown.send(true).ok();
            vec![
                json!({"type":"daemon","status":"stopping","working_agents":manager.working_count()}),
            ]
        }
        Request::AgentSpawn {
            dir,
            message,
            title,
            mode,
            wall_time_hours,
        } => vec![serde_json::to_value(manager.spawn(
            dir,
            message,
            title,
            mode,
            wall_time_hours,
        )?)?],
        Request::AgentList { mut filter } => {
            if filter.limit == 0 {
                filter.limit = 100
            }
            if filter.sort.is_empty() {
                filter.sort = "spawned_at".into()
            }
            if filter.order.is_empty() {
                filter.order = "desc".into()
            }
            if let Some(dir) = filter.dir.take() {
                filter.dir = Some(canonical_dir(&dir)?);
            }
            store
                .list(&filter)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        }
        Request::AgentStatus { id } => vec![serde_json::to_value(store.load_metadata(&id)?)?],
        Request::AgentLogs {
            id,
            types,
            after,
            limit,
            follow,
        } => {
            if follow {
                validate_log_cursor(&store.read_events(&id)?, after.as_deref())?;
                return Ok(Output::Follow {
                    id,
                    types,
                    after,
                    limit,
                });
            }
            select_logs(store.read_events(&id)?, &types, after.as_deref(), limit)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        }
        Request::AgentContext {
            id,
            include,
            max_tokens,
        } => context_lines(&id, store.read_events(&id)?, &include, max_tokens)?,
        Request::AgentSend {
            id,
            message,
            wall_time_hours,
        } => vec![serde_json::to_value(manager.send(
            &id,
            message,
            wall_time_hours,
        )?)?],
        Request::AgentSide {
            id,
            message,
            wall_time_hours,
        } => vec![manager.side(&id, message, wall_time_hours).await?],
        Request::AgentTime { id, hours } => {
            vec![serde_json::to_value(manager.update_time(&id, hours)?)?]
        }
        Request::AgentStop { id } => vec![serde_json::to_value(
            manager.stop(&id, "user_request").await?,
        )?],
        Request::AgentDelete { id } => {
            store.delete(&id)?;
            vec![json!({"type":"agent_deleted","id":id})]
        }
    };
    Ok(Output::Lines(lines))
}

fn select_logs(
    mut events: Vec<EventRecord>,
    types: &[String],
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<EventRecord>> {
    validate_log_cursor(&events, after)?;
    if let Some(after) = after {
        let pos = events.iter().position(|e| e.event_id == after).unwrap();
        events.drain(..=pos);
    }
    if !types.is_empty() {
        events.retain(|e| types.iter().any(|t| t == &e.event_type));
    }
    let limit = if limit == 0 { 100 } else { limit };
    if events.len() > limit {
        events.drain(..events.len() - limit);
    }
    Ok(events)
}

fn validate_log_cursor(events: &[EventRecord], after: Option<&str>) -> Result<()> {
    if let Some(after) = after
        && !events.iter().any(|event| event.event_id == after)
    {
        bail!("event cursor not found: {after}");
    }
    Ok(())
}

fn context_lines(
    id: &str,
    events: Vec<EventRecord>,
    include: &[String],
    max_tokens: usize,
) -> Result<Vec<Value>> {
    let types = if include.is_empty() {
        vec!["user_message".to_string(), "assistant_message".to_string()]
    } else {
        include.to_vec()
    };
    let mut selected: Vec<_> = events
        .into_iter()
        .filter(|e| types.iter().any(|t| t == &e.event_type))
        .collect();
    let selected_count = selected.len();
    let first_user = selected
        .iter()
        .find(|event| event.event_type == "user_message")
        .cloned();
    let max = if max_tokens == 0 { 12_000 } else { max_tokens };
    let mut used = 0usize;
    let mut kept = Vec::new();
    while let Some(e) = selected.pop() {
        let cost = serde_json::to_vec(&e)?.len() / 4;
        if used + cost > max {
            continue;
        }
        used += cost;
        kept.push(e);
    }
    if let Some(first_user) = first_user {
        if !kept
            .iter()
            .any(|event| event.event_id == first_user.event_id)
        {
            let cost = serde_json::to_vec(&first_user)?.len() / 4;
            while used + cost > max && !kept.is_empty() {
                let removed = kept.pop().unwrap();
                used = used.saturating_sub(serde_json::to_vec(&removed)?.len() / 4);
            }
            if used + cost <= max {
                used += cost;
                kept.push(first_user);
            }
        }
    }
    kept.sort_by_key(|event| event.sequence);
    let truncated = kept.len() < selected_count;
    let mut out = vec![
        json!({"type":"context_meta","agent_id":id,"estimated_tokens":used,"max_tokens":max,"truncated":truncated,"included_types":types}),
    ];
    out.extend(kept.into_iter().map(|e| serde_json::to_value(e).unwrap()));
    Ok(out)
}

async fn follow_logs<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    store: &Store,
    id: &str,
    types: &[String],
    after: Option<&str>,
    limit: usize,
) -> Result<()> {
    let mut cursor = after.map(str::to_string);
    let initial = select_logs(store.read_events(id)?, types, after, limit)?;
    for event in initial {
        cursor = Some(event.event_id.clone());
        write_json_line(writer, &serde_json::to_value(event)?).await?;
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let events = select_logs(store.read_events(id)?, types, cursor.as_deref(), usize::MAX)?;
        for event in events {
            cursor = Some(event.event_id.clone());
            if write_json_line(writer, &serde_json::to_value(event)?)
                .await
                .is_err()
            {
                return Ok(());
            }
        }
    }
}

async fn write_json_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<()> {
    let mut body = serde_json::to_vec(value)?;
    body.push(b'\n');
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}
fn error_code(e: &anyhow::Error) -> &'static str {
    let s = e.to_string();
    if s.contains("not found") {
        "not_found"
    } else if s.contains("max agents") {
        "max_agents_reached"
    } else if s.contains("not working") || s.contains("working agent") {
        "conflict"
    } else if s.contains("must") || s.contains("invalid") || s.contains("empty") {
        "invalid_argument"
    } else {
        "internal_error"
    }
}

fn acquire_daemon_lock(path: &Path) -> Result<std::fs::File> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let open = || {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        options.open(path)
    };
    let mut file = match open() {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let pid = std::fs::read_to_string(path)
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok());
            if pid.is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0) {
                anyhow::bail!("daemon is already running");
            }
            std::fs::remove_file(path)?;
            open()?
        }
        Err(error) => return Err(error.into()),
    };
    writeln!(file, "{}", std::process::id())?;
    file.sync_all()?;
    Ok(file)
}

#[cfg(unix)]
fn set_socket_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_socket_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn event(id: &str, sequence: u64, event_type: &str) -> EventRecord {
        EventRecord {
            event_id: id.into(),
            agent_id: "agt_test".into(),
            sequence,
            timestamp: Utc::now(),
            event_type: event_type.into(),
            data: json!({}),
        }
    }

    #[test]
    fn log_cursor_is_exclusive_and_filtering_precedes_limit() {
        let events = vec![
            event("evt_1", 1, "lifecycle"),
            event("evt_2", 2, "assistant_message"),
            event("evt_3", 3, "lifecycle"),
            event("evt_4", 4, "assistant_message"),
        ];
        let selected =
            select_logs(events, &["assistant_message".into()], Some("evt_1"), 1).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].event_id, "evt_4");
    }

    #[test]
    fn unknown_log_cursor_is_rejected() {
        let error = select_logs(
            vec![event("evt_1", 1, "lifecycle")],
            &[],
            Some("evt_missing"),
            100,
        )
        .unwrap_err();
        assert_eq!(error_code(&error), "not_found");
    }
}
