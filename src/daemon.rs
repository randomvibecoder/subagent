use crate::{
    agent::AgentManager,
    config::{RuntimeConfig, ensure_private_dir},
    ipc::{PROTOCOL_VERSION, Request, coded_error, error_json_for},
    store::{AgentStatus, EventRecord, InboxFilter, Store, canonical_filter_dir},
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
    let (web_ui_url, web_auth) = if let Some(port) = web_ui_port {
        let runtime = crate::web::start(
            port,
            manager.clone(),
            store.clone(),
            cfg.web_password.as_deref(),
        )
        .await?;
        (Some(runtime.url), Some(runtime.auth))
    } else {
        (None, None)
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
    let mut stall_tick = tokio::time::interval(std::time::Duration::from_secs(5));
    stall_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;let manager=manager.clone();let store=store.clone();let cfg=cfg.clone();let tx=shutdown_tx.clone();let web_ui_url=web_ui_url.clone();let web_auth=web_auth.clone();
                tokio::spawn(async move{if let Err(e)=handle_connection(stream,manager,store,cfg,tx,web_ui_url,web_auth).await{eprintln!("ipc error: {e:#}");}});
            }
            _=shutdown_rx.changed()=>{if *shutdown_rx.borrow(){break}}
            _=stall_tick.tick()=>{if let Err(error)=manager.check_stalls(){eprintln!("stall watchdog error: {error:#}");}}
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
    web_auth: Option<String>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let line = lines.next_line().await?.context("empty request")?;
    let request: Request = serde_json::from_str(&line).context("invalid request JSON")?;
    match dispatch(
        request,
        &manager,
        &store,
        &cfg,
        &shutdown,
        &web_ui_url,
        &web_auth,
    )
    .await
    {
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
    web_auth: &Option<String>,
) -> Result<Output> {
    let req = resolve_local_references(req, store)?;
    let lines = match req {
        Request::DaemonStatus => vec![
            json!({"type":"daemon","status":"running","version":env!("CARGO_PKG_VERSION"),"protocol_version":PROTOCOL_VERSION,"pid":std::process::id(),"socket":cfg.paths.socket(),"working_agents":manager.working_count(),"max_agents":cfg.file.max_agents,"model":cfg.file.model,"base_url":cfg.file.base_url,"web_ui_url":web_ui_url,"web_auth":web_auth}),
        ],
        Request::DaemonStop => {
            shutdown.send(true).ok();
            vec![
                json!({"type":"daemon","status":"stopping","working_agents":manager.working_count()}),
            ]
        }
        Request::ConfigActive => crate::config::CONFIG_KEYS
            .iter()
            .map(|key| {
                Ok(json!({
                    "type":"active_config_value",
                    "key":key,
                    "active_value":cfg.file.get(key)?,
                    "active_source":cfg.sources.get(*key),
                }))
            })
            .collect::<Result<Vec<_>>>()?,
        Request::AgentSpawn {
            dir,
            message,
            name,
            mode,
            model,
            wall_time_minutes,
        } => vec![agent_value(manager.spawn(
            dir,
            message,
            name,
            mode,
            model,
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
            let mut values = if filter.verbose {
                manager.list_verbose_values(&filter)?
            } else {
                manager
                    .list_items(&filter)?
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?
            };
            values.push(json!({"type":"list_summary","resource":"agents","count":values.len()}));
            values
        }
        Request::AgentStatus { id } => vec![agent_value(store.load_metadata(&id)?)?],
        Request::AgentWait {
            id,
            timeout_seconds,
        } => vec![agent_value(
            wait_for_agent(store, &id, timeout_seconds).await?,
        )?],
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
                query_logs(store, &id, false, &types, after.as_deref(), limit)?;
                return Ok(Output::Follow {
                    id,
                    types,
                    after,
                    limit,
                    side: false,
                });
            }
            query_logs(store, &id, false, &types, after.as_deref(), limit)?
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
            model,
            wall_time_minutes,
        } => vec![manager.create_side(&id, message, model, wall_time_minutes)?],
        Request::Inbox {
            limit,
            offset,
            minimum_priority,
            agent_id,
        } => {
            if !(1..=100).contains(&limit) || !(1..=5).contains(&minimum_priority) {
                return Err(coded_error(
                    "invalid_argument",
                    "inbox limit must be 1..=100 and priority must be 1..=5",
                    json!({"limit":limit,"priority":minimum_priority}),
                    false,
                ));
            }
            store
                .list_notifications(&InboxFilter {
                    limit,
                    offset,
                    minimum_priority,
                    agent_id,
                })?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        }
        Request::SideList {
            agent_id,
            statuses,
            mut limit,
            offset,
        } => {
            if limit == 0 {
                limit = 100;
            }
            let mut values = manager
                .list_sides(&agent_id, &statuses, limit, offset)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            let agent_ref = store.load_metadata(&agent_id)?.local_ref;
            values.push(json!({"type":"list_summary","resource":"sides","agent_id":agent_id,"agent_ref":agent_ref,"count":values.len()}));
            values
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
                query_logs(store, &id, true, &types, after.as_deref(), limit)?;
                return Ok(Output::Follow {
                    id,
                    types,
                    after,
                    limit,
                    side: true,
                });
            }
            query_logs(store, &id, true, &types, after.as_deref(), limit)?
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?
        }
        Request::SideStop { id } => vec![serde_json::to_value(
            manager.stop_side(&id, "user_request").await?,
        )?],
        Request::SideDelete { id } => {
            let side = store.load_side_metadata(&id)?;
            store.delete_side(&id)?;
            vec![
                json!({"type":"side_deleted","id":id,"ref":side.local_ref,"agent_id":side.agent_id,"agent_ref":side.agent_ref}),
            ]
        }
        Request::AgentTime { id, minutes } => {
            vec![agent_value(manager.update_time(&id, minutes)?)?]
        }
        Request::AgentStop { id } => {
            vec![agent_value(manager.stop(&id, "user_request").await?)?]
        }
        Request::AgentDelete { id } => vec![manager.delete_agent(&id).await?],
        Request::MessageList { agent_id, statuses } => {
            let mut values = store
                .read_messages(&agent_id)?
                .into_iter()
                .filter(|message| {
                    statuses.is_empty()
                        || statuses
                            .iter()
                            .any(|status| status == message.status.as_str())
                })
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            let agent_ref = store.load_metadata(&agent_id)?.local_ref;
            values.push(json!({"type":"list_summary","resource":"messages","agent_id":agent_id,"agent_ref":agent_ref,"count":values.len()}));
            values
        }
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

fn resolve_local_references(req: Request, store: &Store) -> Result<Request> {
    Ok(match req {
        Request::AgentStatus { id } => Request::AgentStatus {
            id: store.resolve_agent_id(&id)?,
        },
        Request::AgentWait {
            id,
            timeout_seconds,
        } => Request::AgentWait {
            id: store.resolve_agent_id(&id)?,
            timeout_seconds,
        },
        Request::AgentRename { id, name } => Request::AgentRename {
            id: store.resolve_agent_id(&id)?,
            name,
        },
        Request::AgentContext { id } => Request::AgentContext {
            id: store.resolve_agent_id(&id)?,
        },
        Request::AgentSend {
            id,
            message,
            wall_time_minutes,
        } => Request::AgentSend {
            id: store.resolve_agent_id(&id)?,
            message,
            wall_time_minutes,
        },
        Request::AgentSide {
            id,
            message,
            model,
            wall_time_minutes,
        } => Request::AgentSide {
            id: store.resolve_agent_id(&id)?,
            message,
            model,
            wall_time_minutes,
        },
        Request::AgentTime { id, minutes } => Request::AgentTime {
            id: store.resolve_agent_id(&id)?,
            minutes,
        },
        Request::AgentStop { id } => Request::AgentStop {
            id: store.resolve_agent_id(&id)?,
        },
        Request::AgentDelete { id } => Request::AgentDelete {
            id: store.resolve_agent_id(&id)?,
        },
        Request::AgentLogs {
            id,
            types,
            all,
            after,
            limit,
            follow,
        } => {
            let id = store.resolve_agent_id(&id)?;
            let after = after
                .map(|cursor| store.resolve_event_id(&id, false, &cursor))
                .transpose()?;
            Request::AgentLogs {
                id,
                types,
                all,
                after,
                limit,
                follow,
            }
        }
        Request::SideList {
            agent_id,
            statuses,
            limit,
            offset,
        } => Request::SideList {
            agent_id: store.resolve_agent_id(&agent_id)?,
            statuses,
            limit,
            offset,
        },
        Request::SideStatus { id } => Request::SideStatus {
            id: store.resolve_side_id(&id)?,
        },
        Request::SideStop { id } => Request::SideStop {
            id: store.resolve_side_id(&id)?,
        },
        Request::SideDelete { id } => Request::SideDelete {
            id: store.resolve_side_id(&id)?,
        },
        Request::SideLogs {
            id,
            types,
            all,
            after,
            limit,
            follow,
        } => {
            let id = store.resolve_side_id(&id)?;
            let after = after
                .map(|cursor| store.resolve_event_id(&id, true, &cursor))
                .transpose()?;
            Request::SideLogs {
                id,
                types,
                all,
                after,
                limit,
                follow,
            }
        }
        Request::MessageList { agent_id, statuses } => Request::MessageList {
            agent_id: store.resolve_agent_id(&agent_id)?,
            statuses,
        },
        Request::MessageStatus {
            agent_id,
            message_id,
        } => {
            let agent_id = store.resolve_agent_id(&agent_id)?;
            let message_id = store.resolve_message_id(&agent_id, &message_id)?;
            Request::MessageStatus {
                agent_id,
                message_id,
            }
        }
        Request::MessageCancel {
            agent_id,
            message_id,
        } => {
            let agent_id = store.resolve_agent_id(&agent_id)?;
            let message_id = store.resolve_message_id(&agent_id, &message_id)?;
            Request::MessageCancel {
                agent_id,
                message_id,
            }
        }
        Request::Inbox {
            limit,
            offset,
            minimum_priority,
            agent_id,
        } => Request::Inbox {
            limit,
            offset,
            minimum_priority,
            agent_id: agent_id.map(|id| store.resolve_agent_id(&id)).transpose()?,
        },
        other => other,
    })
}

async fn wait_for_agent(
    store: &Store,
    id: &str,
    timeout_seconds: Option<u64>,
) -> Result<crate::store::AgentMetadata> {
    let started = tokio::time::Instant::now();
    loop {
        let meta = store.load_metadata(id)?;
        if meta.status != AgentStatus::Working {
            return Ok(meta);
        }
        if timeout_seconds.is_some_and(|seconds| started.elapsed().as_secs() >= seconds) {
            return Err(coded_error(
                "timeout",
                format!("agent wait timed out: {id}"),
                json!({"agent_id":id,"timeout_seconds":timeout_seconds,"status":"working"}),
                true,
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn agent_value(meta: crate::store::AgentMetadata) -> Result<Value> {
    let mut value = serde_json::to_value(meta)?;
    value.as_object_mut().unwrap().remove("name");
    Ok(value)
}

fn query_logs(
    store: &Store,
    id: &str,
    side: bool,
    types: &[String],
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<EventRecord>> {
    let limit = if limit == 0 { 100 } else { limit };
    store
        .query_events(id, side, types, after, None, limit)
        .map_err(|error| {
            if let Some(after) = after
                && error.to_string().contains("event cursor not found")
            {
                return coded_error(
                    "event_not_found",
                    format!("event cursor not found: {after}"),
                    json!({"event_id":after}),
                    false,
                );
            }
            error
        })
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
    let initial = query_logs(store, id, side, types, after, limit)?;
    for event in initial {
        cursor = Some(event.event_id.clone());
        write_json_line(writer, &serde_json::to_value(event)?).await?;
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let events = query_logs(store, id, side, types, cursor.as_deref(), 10_000)?;
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
