use crate::{
    agent::AgentManager,
    config::{RuntimeConfig, ensure_private_dir},
    ipc::{Request, coded_error, error_json_for},
    store::{AgentStatus, EventRecord, Store, canonical_filter_dir},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::watch,
};

pub async fn serve(cfg: RuntimeConfig, web_ui_port: Option<u16>) -> Result<()> {
    ensure_private_dir(&cfg.paths.runtime_dir)?;
    let lock_path = cfg.paths.daemon_lock();
    let _lock = acquire_daemon_lock(&lock_path)?;
    let store = Store::new(&cfg.paths)?;
    let recovered = store.recover_interrupted()?;
    let recovered_sides = store.recover_interrupted_sides()?;
    let cfg = Arc::new(cfg);
    let manager = AgentManager::new(cfg.clone(), store.clone());
    manager.schedule_pending()?;
    let web_ui_url = if let Some(port) = web_ui_port {
        Some(
            crate::web::start(port, manager.clone(), store.clone())
                .await?
                .url,
        )
    } else {
        None
    };
    let socket = cfg.paths.socket();
    if socket.exists() {
        std::fs::remove_file(&socket).context("remove stale daemon socket")?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    set_socket_permissions(&socket)?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    if recovered > 0 || recovered_sides > 0 {
        eprintln!(
            "recovered {recovered} interrupted agents and {recovered_sides} side runs as stopped"
        );
    }
    loop {
        tokio::select! {
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;let manager=manager.clone();let store=store.clone();let cfg=cfg.clone();let tx=shutdown_tx.clone();let web_ui_url=web_ui_url.clone();
                tokio::spawn(async move{if let Err(e)=handle_connection(stream,manager,store,cfg,tx,web_ui_url).await{eprintln!("ipc error: {e:#}");}});
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
    web_ui_url: Option<String>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let line = lines.next_line().await?.context("empty request")?;
    let request: Request = serde_json::from_str(&line).context("invalid request JSON")?;
    match dispatch(request, &manager, &store, &cfg, &shutdown, &web_ui_url).await {
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
            side,
        }) => {
            follow_logs(
                &mut write_half,
                &store,
                &id,
                &types,
                after.as_deref(),
                limit,
                side,
            )
            .await?
        }
        Err(e) => write_json_line(&mut write_half, &error_json_for(&e, "internal_error")).await?,
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
        side: bool,
    },
}

async fn dispatch(
    req: Request,
    manager: &AgentManager,
    store: &Store,
    cfg: &RuntimeConfig,
    shutdown: &watch::Sender<bool>,
    web_ui_url: &Option<String>,
) -> Result<Output> {
    let lines = match req {
        Request::DaemonStatus => vec![
            json!({"type":"daemon","status":"running","pid":std::process::id(),"socket":cfg.paths.socket(),"working_agents":manager.working_count(),"max_agents":cfg.file.max_agents,"model":cfg.file.model,"base_url":cfg.file.base_url,"web_ui_url":web_ui_url}),
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
            name,
            mode,
            wall_time_minutes,
        } => vec![agent_value(manager.spawn(
            dir,
            message,
            name,
            mode,
            wall_time_minutes,
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
                filter.dir = Some(canonical_filter_dir(&dir)?);
            }
            manager
                .list_items(&filter)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        }
        Request::AgentStatus { id } => vec![agent_value(store.load_metadata(&id)?)?],
        Request::AgentRename { id, name } => vec![manager.rename(&id, name)?],
        Request::AgentLogs {
            id,
            mut types,
            all,
            after,
            limit,
            follow,
        } => {
            if !all && types.is_empty() {
                types = vec![
                    "system_message".into(),
                    "user_message".into(),
                    "assistant_message".into(),
                ];
            }
            if follow {
                validate_log_cursor(&store.read_events(&id)?, after.as_deref())?;
                return Ok(Output::Follow {
                    id,
                    types,
                    after,
                    limit,
                    side: false,
                });
            }
            select_logs(store.read_events(&id)?, &types, after.as_deref(), limit)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        }
        Request::AgentContext { id } => context_lines(&id, store.load_context(&id)?)?,
        Request::AgentSend {
            id,
            message,
            wall_time_minutes,
        } => vec![manager.send(&id, message, wall_time_minutes)?],
        Request::AgentSide {
            id,
            message,
            wall_time_minutes,
        } => vec![manager.create_side(&id, message, wall_time_minutes)?],
        Request::SideList {
            agent_id,
            statuses,
            mut limit,
            offset,
        } => {
            if limit == 0 {
                limit = 100;
            }
            manager
                .list_sides(&agent_id, &statuses, limit, offset)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        }
        Request::SideStatus { id } => vec![serde_json::to_value(store.load_side_metadata(&id)?)?],
        Request::SideLogs {
            id,
            mut types,
            all,
            after,
            limit,
            follow,
        } => {
            if !all && types.is_empty() {
                types = vec!["user_message".into(), "assistant_message".into()];
            }
            if follow {
                validate_log_cursor(&store.read_side_events(&id)?, after.as_deref())?;
                return Ok(Output::Follow {
                    id,
                    types,
                    after,
                    limit,
                    side: true,
                });
            }
            select_logs(
                store.read_side_events(&id)?,
                &types,
                after.as_deref(),
                limit,
            )?
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()?
        }
        Request::SideStop { id } => vec![serde_json::to_value(
            manager.stop_side(&id, "user_request").await?,
        )?],
        Request::SideDelete { id } => {
            store.delete_side(&id)?;
            vec![json!({"type":"side_deleted","id":id})]
        }
        Request::AgentTime { id, minutes } => {
            vec![agent_value(manager.update_time(&id, minutes)?)?]
        }
        Request::AgentStop { id } => {
            vec![agent_value(manager.stop(&id, "user_request").await?)?]
        }
        Request::AgentDelete { id } => vec![manager.delete_agent(&id).await?],
        Request::MessageList { agent_id, statuses } => store
            .read_messages(&agent_id)?
            .into_iter()
            .filter(|message| {
                statuses.is_empty()
                    || statuses
                        .iter()
                        .any(|status| status == message.status.as_str())
            })
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()?,
        Request::MessageStatus {
            agent_id,
            message_id,
        } => {
            vec![serde_json::to_value(
                store.load_message(&agent_id, &message_id)?,
            )?]
        }
        Request::MessageCancel {
            agent_id,
            message_id,
        } => {
            vec![serde_json::to_value(
                manager.cancel_message(&agent_id, &message_id)?,
            )?]
        }
    };
    Ok(Output::Lines(lines))
}

fn agent_value(meta: crate::store::AgentMetadata) -> Result<Value> {
    let mut value = serde_json::to_value(meta)?;
    value.as_object_mut().unwrap().remove("name");
    Ok(value)
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
        return Err(coded_error(
            "event_not_found",
            format!("event cursor not found: {after}"),
            json!({"event_id":after}),
            false,
        ));
    }
    Ok(())
}

fn context_lines(id: &str, context: crate::store::ContextSnapshot) -> Result<Vec<Value>> {
    let mut out = vec![json!({
        "type":"context_meta",
        "agent_id":id,
        "message_count":context.messages.len(),
        "compacted_at":context.compacted_at,
    })];
    out.extend(context.messages);
    Ok(out)
}

async fn follow_logs<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    store: &Store,
    id: &str,
    types: &[String],
    after: Option<&str>,
    limit: usize,
    side: bool,
) -> Result<()> {
    let mut cursor = after.map(str::to_string);
    let read = || {
        if side {
            store.read_side_events(id)
        } else {
            store.read_events(id)
        }
    };
    let initial = select_logs(read()?, types, after, limit)?;
    for event in initial {
        cursor = Some(event.event_id.clone());
        write_json_line(writer, &serde_json::to_value(event)?).await?;
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let events = select_logs(read()?, types, cursor.as_deref(), usize::MAX)?;
        for event in events {
            cursor = Some(event.event_id.clone());
            if write_json_line(writer, &serde_json::to_value(event)?)
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        let status = if side {
            store.load_side_metadata(id)?.status
        } else {
            store.load_metadata(id)?.status
        };
        if status != AgentStatus::Working {
            return Ok(());
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
            side_id: None,
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
        assert_eq!(
            error.downcast_ref::<crate::ipc::CodedError>().unwrap().code,
            "event_not_found"
        );
    }
}
