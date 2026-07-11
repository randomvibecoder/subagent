use crate::{
    agent::AgentManager,
    ipc::{AgentMode, ListFilter, error_json_for},
    store::{AgentStatus, EventRecord, Store},
};
use anyhow::{Result, anyhow};
use async_stream::stream;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, net::Ipv4Addr, sync::Arc, time::Duration};

const INDEX: &str = include_str!("../web/index.html");
const CSS: &str = include_str!("../web/app.css");
const JS: &str = include_str!("../web/app.js");

#[derive(Clone)]
struct WebState {
    manager: AgentManager,
    store: Store,
    token: Arc<String>,
    origin: Arc<String>,
}

pub struct WebRuntime {
    pub url: String,
}

pub async fn start(port: u16, manager: AgentManager, store: Store) -> Result<WebRuntime> {
    let token = format!("web_{}", ulid::Ulid::new());
    let origin = format!("http://127.0.0.1:{port}");
    let url = format!("{origin}/#token={token}");
    let state = WebState {
        manager,
        store,
        token: Arc::new(token),
        origin: Arc::new(origin),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/assets/app.css", get(css))
        .route("/assets/ui-core.js", get(ui_core))
        .route("/assets/app.js", get(js))
        .route("/api/agents", get(list_agents).post(spawn_agent))
        .route("/api/agents/{id}", get(agent_status).delete(delete_agent))
        .route("/api/agents/{id}/rename", post(rename_agent))
        .route("/api/agents/{id}/events", get(agent_events))
        .route("/api/agents/{id}/events/{event_id}", get(full_event))
        .route("/api/agents/{id}/stream", get(event_stream))
        .route("/api/agents/{id}/send", post(send_message))
        .route("/api/agents/{id}/side", post(side_question))
        .route("/api/agents/{id}/sides", get(list_sides).post(create_side))
        .route("/api/sides/{id}", get(side_status).delete(delete_side))
        .route("/api/sides/{id}/stop", post(stop_side))
        .route("/api/sides/{id}/events", get(side_events))
        .route("/api/sides/{id}/events/{event_id}", get(full_side_event))
        .route("/api/sides/{id}/stream", get(side_event_stream))
        .route("/api/agents/{id}/time", post(set_time))
        .route("/api/agents/{id}/stop", post(stop_agent))
        .route("/api/agents/{id}/messages", get(list_messages))
        .route(
            "/api/agents/{id}/messages/{message_id}",
            get(message_status),
        )
        .route(
            "/api/agents/{id}/messages/{message_id}/cancel",
            post(cancel_message),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|e| anyhow!("web UI could not bind 127.0.0.1:{port}: {e}"))?;
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("web UI error: {error}");
        }
    });
    Ok(WebRuntime { url })
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX)
}
async fn css() -> Response {
    asset("text/css; charset=utf-8", CSS)
}
async fn ui_core() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../web/ui-core.js"),
    )
}
async fn js() -> Response {
    asset("text/javascript; charset=utf-8", JS)
}

fn asset(content_type: &'static str, content: &'static str) -> Response {
    let mut response = ([(header::CONTENT_TYPE, content_type)], content).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; connect-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'"),
    );
    response
}

struct ApiError(anyhow::Error);
impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        Self(value.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        if message == "unauthorized" {
            return (StatusCode::UNAUTHORIZED, Json(json!({"type":"error","code":"unauthorized","message":message,"details":{},"retryable":false}))).into_response();
        }
        if message == "invalid origin" {
            return (StatusCode::FORBIDDEN, Json(json!({"type":"error","code":"invalid_origin","message":message,"details":{},"retryable":false}))).into_response();
        }
        let value = error_json_for(&self.0, "internal_error");
        let status = match value.get("code").and_then(Value::as_str) {
            Some("not_found") => StatusCode::NOT_FOUND,
            Some("conflict") | Some("max_agents_reached") => StatusCode::CONFLICT,
            Some("invalid_argument") => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(value)).into_response()
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;

fn authorize(state: &WebState, headers: &HeaderMap, mutation: bool) -> ApiResult<()> {
    let expected = format!("Bearer {}", state.token);
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return Err(ApiError(anyhow!("unauthorized")));
    }
    if mutation
        && headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(state.origin.as_str())
    {
        return Err(ApiError(anyhow!("invalid origin")));
    }
    Ok(())
}

fn ndjson(values: impl IntoIterator<Item = Value>) -> Response {
    let body = values
        .into_iter()
        .map(|v| serde_json::to_string(&v).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    (
        [(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn list_agents(State(state): State<WebState>, headers: HeaderMap) -> ApiResult<Response> {
    authorize(&state, &headers, false)?;
    let values = state
        .manager
        .list_items(&ListFilter {
            limit: 1000,
            sort: "spawned_at".into(),
            order: "desc".into(),
            ..Default::default()
        })?
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ndjson(values))
}

#[derive(Deserialize)]
struct SpawnBody {
    dir: String,
    message: String,
    name: String,
    #[serde(default)]
    mode: Option<AgentMode>,
    wall_time_minutes: Option<u64>,
}
async fn spawn_agent(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(body): Json<SpawnBody>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    let meta = state.manager.spawn(
        body.dir,
        body.message,
        body.name,
        body.mode.unwrap_or(AgentMode::Write),
        body.wall_time_minutes,
    )?;
    Ok(Json(agent_value(meta)?))
}

async fn agent_status(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, false)?;
    Ok(Json(agent_value(state.store.load_metadata(&id)?)?))
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}
async fn rename_agent(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(state.manager.rename(&id, body.name)?))
}

async fn delete_agent(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(state.manager.delete_agent(&id).await?))
}

#[derive(Default, Deserialize)]
struct EventsQuery {
    before: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
    types: Option<String>,
}
async fn agent_events(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Response> {
    authorize(&state, &headers, false)?;
    let mut events = state.store.read_events(&id)?;
    if let Some(before) = query.before {
        let pos = events
            .iter()
            .position(|e| e.event_id == before)
            .ok_or_else(|| anyhow!("event cursor not found"))?;
        events.truncate(pos);
    }
    if let Some(after) = query.after {
        let pos = events
            .iter()
            .position(|e| e.event_id == after)
            .ok_or_else(|| anyhow!("event cursor not found"))?;
        events.drain(..=pos);
    }
    let types = event_types(query.types.as_deref());
    events.retain(|e| types.is_empty() || types.iter().any(|t| t == &e.event_type));
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    if events.len() > limit {
        events.drain(..events.len() - limit);
    }
    Ok(ndjson(events.iter().map(event_preview)))
}

async fn full_event(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path((id, event_id)): Path<(String, String)>,
) -> ApiResult<Json<EventRecord>> {
    authorize(&state, &headers, false)?;
    let event = state
        .store
        .read_events(&id)?
        .into_iter()
        .find(|e| e.event_id == event_id)
        .ok_or_else(|| anyhow!("event not found"))?;
    Ok(Json(event))
}

async fn event_stream(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>> {
    authorize(&state, &headers, false)?;
    state.store.load_metadata(&id)?;
    let mut cursor = query.after;
    if cursor.is_none() {
        cursor = state
            .store
            .read_events(&id)?
            .last()
            .map(|e| e.event_id.clone());
    }
    let types = event_types(query.types.as_deref());
    let output = stream! {
        loop {
            let events = state.store.read_events(&id).unwrap_or_default();
            let start = cursor.as_ref().and_then(|c| events.iter().position(|e| &e.event_id == c)).map(|p| p + 1).unwrap_or(0);
            for event in events.iter().skip(start) {
                cursor = Some(event.event_id.clone());
                if types.is_empty() || types.iter().any(|t| t == &event.event_type) {
                    yield Ok(Event::default().id(event.event_id.clone()).event("event").data(serde_json::to_string(&event_preview(event)).unwrap()));
                }
            }
            if state.store.load_metadata(&id).map(|m| m.status != AgentStatus::Working).unwrap_or(true) { break; }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    Ok(Sse::new(output).keep_alive(axum::response::sse::KeepAlive::default()))
}

fn event_types(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("system_message,user_message,assistant_message,tool_call,tool_result")
        .split(',')
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

fn event_preview(event: &EventRecord) -> Value {
    let mut value = serde_json::to_value(event).unwrap();
    match event.event_type.as_str() {
        "tool_call" => {
            let name = event.data.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = event
                .data
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            if name == "apply_patch"
                && let Ok(arguments_value) = serde_json::from_str::<Value>(arguments)
                && let Some(patch) = arguments_value.get("patch").and_then(Value::as_str)
            {
                let (preview, truncated) = utf8_preview(patch, 4096);
                value["data"] = json!({
                    "tool_call_id":event.data.get("tool_call_id"),
                    "name":name,
                    "patch_preview":preview,
                    "preview_truncated":truncated,
                    "has_full_payload":true
                });
            } else {
                let (preview, truncated) = utf8_preview(arguments, 4096);
                value["data"]["arguments"] = Value::String(preview.to_owned());
                value["data"]["preview_truncated"] = Value::Bool(truncated);
                value["data"]["has_full_payload"] = Value::Bool(true);
            }
        }
        "tool_result" => {
            let result = event.data.get("result").cloned().unwrap_or(Value::Null);
            let output = result
                .get("output")
                .and_then(|v| v.get("content"))
                .and_then(Value::as_str)
                .or_else(|| result.get("content").and_then(Value::as_str));
            let (output_preview, output_truncated) =
                output.map(|v| utf8_preview(v, 4096)).unwrap_or(("", false));
            value["data"] = json!({
                "tool_call_id":event.data.get("tool_call_id"),
                "name":event.data.get("name"),
                "summary":{
                    "ok":result.get("ok"),
                    "status":result.get("status"),
                    "exit_code":result.get("exit_code"),
                    "path":result.get("path"),
                    "bytes":result.get("bytes"),
                    "truncated":result.get("truncated"),
                    "output_preview":output_preview,
                    "output_truncated":output_truncated
                },
                "has_full_payload":true
            });
        }
        _ => {
            for field in ["content", "error"] {
                if let Some(text) = value["data"].get(field).and_then(Value::as_str) {
                    let (preview, truncated) = utf8_preview(text, 4096);
                    value["data"][field] = Value::String(preview.to_owned());
                    if truncated {
                        value["data"]["preview_truncated"] = Value::Bool(true);
                        value["data"]["has_full_payload"] = Value::Bool(true);
                    }
                }
            }
        }
    }
    value
}

fn utf8_preview(value: &str, max: usize) -> (&str, bool) {
    if value.len() <= max {
        return (value, false);
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

#[derive(Deserialize)]
struct MessageBody {
    message: String,
    wall_time_minutes: Option<u64>,
}
async fn send_message(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<MessageBody>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(state.manager.send(
        &id,
        body.message,
        body.wall_time_minutes,
    )?))
}
async fn side_question(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<MessageBody>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(state.manager.create_side(
        &id,
        body.message,
        body.wall_time_minutes.or(Some(2)),
    )?))
}

#[derive(Default, Deserialize)]
struct SidesQuery {
    statuses: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn list_sides(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<SidesQuery>,
) -> ApiResult<Response> {
    authorize(&state, &headers, false)?;
    let statuses = query
        .statuses
        .unwrap_or_default()
        .split(',')
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let values = state
        .manager
        .list_sides(
            &id,
            &statuses,
            query.limit.unwrap_or(100).clamp(1, 1000),
            query.offset.unwrap_or(0),
        )?
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ndjson(values))
}

async fn create_side(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<MessageBody>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(state.manager.create_side(
        &id,
        body.message,
        body.wall_time_minutes.or(Some(2)),
    )?))
}

async fn side_status(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::store::SideMetadata>> {
    authorize(&state, &headers, false)?;
    Ok(Json(state.store.load_side_metadata(&id)?))
}

async fn stop_side(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::store::SideMetadata>> {
    authorize(&state, &headers, true)?;
    Ok(Json(state.manager.stop_side(&id, "user_request").await?))
}

async fn delete_side(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    state.store.delete_side(&id)?;
    Ok(Json(json!({"type":"side_deleted","id":id})))
}

async fn side_events(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Response> {
    authorize(&state, &headers, false)?;
    let mut events = state.store.read_side_events(&id)?;
    if let Some(before) = query.before {
        let position = events
            .iter()
            .position(|event| event.event_id == before)
            .ok_or_else(|| anyhow!("event cursor not found"))?;
        events.truncate(position);
    }
    if let Some(after) = query.after {
        let position = events
            .iter()
            .position(|event| event.event_id == after)
            .ok_or_else(|| anyhow!("event cursor not found"))?;
        events.drain(..=position);
    }
    let types = event_types(query.types.as_deref());
    events.retain(|event| types.is_empty() || types.iter().any(|kind| kind == &event.event_type));
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    if events.len() > limit {
        events.drain(..events.len() - limit);
    }
    Ok(ndjson(events.iter().map(event_preview)))
}

async fn full_side_event(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path((id, event_id)): Path<(String, String)>,
) -> ApiResult<Json<EventRecord>> {
    authorize(&state, &headers, false)?;
    let event = state
        .store
        .read_side_events(&id)?
        .into_iter()
        .find(|event| event.event_id == event_id)
        .ok_or_else(|| anyhow!("event not found"))?;
    Ok(Json(event))
}

async fn side_event_stream(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>> {
    authorize(&state, &headers, false)?;
    state.store.load_side_metadata(&id)?;
    let mut cursor = query.after;
    if cursor.is_none() {
        cursor = state
            .store
            .read_side_events(&id)?
            .last()
            .map(|event| event.event_id.clone());
    }
    let types = event_types(query.types.as_deref());
    let output = stream! {
        loop {
            let events = state.store.read_side_events(&id).unwrap_or_default();
            let start = cursor.as_ref().and_then(|value| events.iter().position(|event| &event.event_id == value)).map(|position| position + 1).unwrap_or(0);
            for event in events.iter().skip(start) {
                cursor = Some(event.event_id.clone());
                if types.is_empty() || types.iter().any(|kind| kind == &event.event_type) {
                    yield Ok(Event::default().id(event.event_id.clone()).event("event").data(serde_json::to_string(&event_preview(event)).unwrap()));
                }
            }
            if state.store.load_side_metadata(&id).map(|meta| meta.status != AgentStatus::Working).unwrap_or(true) { break; }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    Ok(Sse::new(output).keep_alive(axum::response::sse::KeepAlive::default()))
}
#[derive(Deserialize)]
struct TimeBody {
    minutes: u64,
}
async fn set_time(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TimeBody>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(agent_value(
        state.manager.update_time(&id, body.minutes)?,
    )?))
}
async fn stop_agent(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(agent_value(
        state.manager.stop(&id, "user_request").await?,
    )?))
}
async fn list_messages(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    authorize(&state, &headers, false)?;
    let values = state
        .store
        .read_messages(&id)?
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ndjson(values))
}
async fn message_status(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path((id, message_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, false)?;
    Ok(Json(serde_json::to_value(
        state.store.load_message(&id, &message_id)?,
    )?))
}
async fn cancel_message(
    State(state): State<WebState>,
    headers: HeaderMap,
    Path((id, message_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    authorize(&state, &headers, true)?;
    Ok(Json(serde_json::to_value(
        state.manager.cancel_message(&id, &message_id)?,
    )?))
}

fn agent_value(meta: crate::store::AgentMetadata) -> Result<Value> {
    let mut value = serde_json::to_value(meta)?;
    value.as_object_mut().unwrap().remove("name");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::event_preview;
    use crate::store::EventRecord;
    use chrono::Utc;
    use serde_json::json;

    fn event(event_type: &str, data: serde_json::Value) -> EventRecord {
        EventRecord {
            event_id: "evt_test".into(),
            agent_id: "agt_test".into(),
            side_id: None,
            sequence: 1,
            timestamp: Utc::now(),
            event_type: event_type.into(),
            data,
        }
    }

    #[test]
    fn apply_patch_preview_is_structured_for_diff_rendering() {
        let patch = "*** Begin Patch\n*** Update File: a\n@@\n-old\n+new\n*** End Patch";
        let preview = event_preview(&event(
            "tool_call",
            json!({"tool_call_id":"call_1","name":"apply_patch","arguments":serde_json::to_string(&json!({"patch":patch})).unwrap()}),
        ));
        assert_eq!(preview["data"]["name"], "apply_patch");
        assert_eq!(preview["data"]["patch_preview"], patch);
        assert!(preview["data"].get("arguments").is_none());
    }

    #[test]
    fn tool_result_preview_omits_the_raw_result_object() {
        let preview = event_preview(&event(
            "tool_result",
            json!({"tool_call_id":"call_1","name":"exec_command","result":{"ok":true,"status":"completed","exit_code":0,"output":{"content":"done"}}}),
        ));
        assert_eq!(preview["data"]["name"], "exec_command");
        assert_eq!(preview["data"]["summary"]["output_preview"], "done");
        assert!(preview["data"].get("result").is_none());
    }
}
