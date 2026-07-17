use crate::{ipc::AgentMode, model::ToolCall, store::Store};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use globset::{Glob, GlobSetBuilder};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, Command},
    sync::Mutex as AsyncMutex,
};

const MAX_TERMINALS: usize = 8;
const MAX_READ_BYTES: usize = 64 * 1024;

pub struct ToolResult {
    pub content: Value,
    pub image_message: Option<Value>,
}

impl ToolResult {
    fn plain(content: Value) -> Self {
        Self {
            content,
            image_message: None,
        }
    }
}

#[derive(Clone)]
pub struct ToolRuntime {
    pub agent_id: String,
    pub cwd: PathBuf,
    pub mode: AgentMode,
    pub store: Store,
    pub terminals: TerminalManager,
    pub preview_bytes: usize,
}

impl ToolRuntime {
    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self
            .execute_inner(&call.function.name, &call.function.arguments)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                ToolResult::plain(json!({"ok":false,"code":"tool_error","error":format!("{e:#}")}))
            }
        }
    }

    async fn execute_inner(&self, name: &str, arguments: &str) -> Result<ToolResult> {
        let args: Value =
            serde_json::from_str(arguments).context("tool arguments must be valid JSON")?;
        match name {
            "read" => self.read(&args),
            "glob" => self.glob(&args),
            "grep" => self.grep(&args),
            "write" => {
                self.require_write()?;
                self.write(&args)
            }
            "edit" => {
                self.require_write()?;
                self.edit(&args)
            }
            "apply_patch" => {
                self.require_write()?;
                self.apply_patch(&args)
            }
            "exec_command" => self.exec_command(&args).await,
            "write_stdin" => self.write_stdin(&args).await,
            "list_terminals" => Ok(ToolResult::plain(self.terminals.list())),
            "terminate_terminal" => self.terminate_terminal(&args).await,
            "terminate_all_terminals" => {
                self.terminals.terminate_all().await?;
                Ok(ToolResult::plain(json!({"ok":true})))
            }
            "view_image" => self.view_image(&args),
            "read_output" => self.read_output(&args),
            "notify" => self.notify(&args),
            _ => bail!("unknown tool: {name}"),
        }
    }

    fn require_write(&self) -> Result<()> {
        if self.mode == AgentMode::Readonly {
            bail!("tool is unavailable in readonly mode");
        }
        Ok(())
    }

    fn notify(&self, args: &Value) -> Result<ToolResult> {
        let event_type = required_str(args, "event_type")?;
        let priority = match event_type {
            "progress" => 1,
            "milestone" => 2,
            "input_required" => 3,
            "blocked" => 4,
            _ => bail!("event_type must be progress, milestone, input_required, or blocked"),
        };
        let summary = required_str(args, "summary")?.trim();
        if summary.is_empty() {
            bail!("summary must not be empty");
        }
        if summary.chars().count() > 5_000 {
            bail!("summary must contain at most 5000 characters");
        }
        let status = if self.agent_id.starts_with("side_") {
            self.store.load_side_metadata(&self.agent_id)?.status
        } else {
            self.store.load_metadata(&self.agent_id)?.status
        };
        let notification = self.store.append_notification_payload(
            &self.agent_id,
            event_type,
            priority,
            status,
            summary,
            Some(json!({"summary":summary})),
        )?;
        Ok(ToolResult::plain(json!({
            "ok":true,
            "notification_id":notification.id,
            "priority":notification.priority,
            "event_type":notification.event_type
        })))
    }

    fn path(&self, args: &Value) -> Result<PathBuf> {
        let raw = required_str(args, "path")?;
        Ok(if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.cwd.join(raw)
        })
    }

    fn read(&self, args: &Value) -> Result<ToolResult> {
        let path = self.path(args)?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .clamp(1, 2000) as usize;
        let file = fs::File::open(&path).with_context(|| format!("read {}", path.display()))?;
        let mut lines = Vec::new();
        let mut bytes = 0;
        for (idx, line) in BufReader::new(file)
            .lines()
            .enumerate()
            .skip(offset - 1)
            .take(limit)
        {
            let line = line?;
            bytes += line.len();
            if bytes > MAX_READ_BYTES {
                break;
            }
            lines.push(format!("{}: {}", idx + 1, line));
        }
        Ok(ToolResult::plain(
            json!({"ok":true,"path":path,"offset":offset,"lines":lines,"truncated":bytes > MAX_READ_BYTES}),
        ))
    }

    fn glob(&self, args: &Value) -> Result<ToolResult> {
        let pattern = required_str(args, "pattern")?;
        let root = args
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .map(|p| if p.is_absolute() { p } else { self.cwd.join(p) })
            .unwrap_or_else(|| self.cwd.clone());
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .clamp(1, 5000) as usize;
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new(pattern)?);
        let set = builder.build()?;
        let mut paths = Vec::new();
        for entry in ignore::WalkBuilder::new(&root)
            .follow_links(false)
            .build()
            .filter_map(Result::ok)
        {
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if rel.as_os_str().is_empty() {
                continue;
            }
            if set.is_match(rel) {
                let path = rel.to_string_lossy().into_owned();
                if path.is_empty() {
                    continue;
                }
                paths.push(path);
                if paths.len() >= limit {
                    break;
                }
            }
        }
        Ok(ToolResult::plain(
            json!({"ok":true,"root":root,"paths":paths,"truncated":paths.len() >= limit}),
        ))
    }

    fn grep(&self, args: &Value) -> Result<ToolResult> {
        let pattern = required_str(args, "pattern")?;
        let regex = Regex::new(pattern)?;
        let root = args
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .map(|p| if p.is_absolute() { p } else { self.cwd.join(p) })
            .unwrap_or_else(|| self.cwd.clone());
        let include = args.get("include").and_then(Value::as_str);
        let include = include
            .map(Glob::new)
            .transpose()?
            .map(|g| {
                let mut b = GlobSetBuilder::new();
                b.add(g);
                b.build()
            })
            .transpose()?;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(200)
            .clamp(1, 2000) as usize;
        let mut matches = Vec::new();
        'files: for entry in ignore::WalkBuilder::new(&root)
            .follow_links(false)
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|kind| kind.is_file()))
        {
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if include.as_ref().is_some_and(|g| !g.is_match(rel)) {
                continue;
            }
            let Ok(file) = fs::File::open(entry.path()) else {
                continue;
            };
            for (idx, line) in BufReader::new(file).lines().enumerate() {
                let Ok(line) = line else { break };
                if regex.is_match(&line) {
                    matches
                        .push(json!({"path":rel,"line":idx+1,"text":truncate_utf8(&line, 2000)}));
                    if matches.len() >= limit {
                        break 'files;
                    }
                }
            }
        }
        Ok(ToolResult::plain(
            json!({"ok":true,"root":root,"matches":matches,"truncated":matches.len() >= limit}),
        ))
    }

    fn write(&self, args: &Value) -> Result<ToolResult> {
        let path = self.path(args)?;
        let content = required_str(args, "content")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(ToolResult::plain(
            json!({"ok":true,"path":path,"bytes":content.len()}),
        ))
    }

    fn edit(&self, args: &Value) -> Result<ToolResult> {
        let path = self.path(args)?;
        let old = required_str(args, "old_text")?;
        if old.is_empty() {
            bail!("old_text must not be empty");
        }
        let new = required_str(args, "new_text")?;
        let expected = args
            .get("expected_replacements")
            .and_then(Value::as_u64)
            .unwrap_or(1) as usize;
        let body = fs::read_to_string(&path)?;
        let found = body.matches(old).count();
        if found != expected {
            bail!("expected {expected} replacements but found {found}");
        }
        fs::write(&path, body.replace(old, new))?;
        Ok(ToolResult::plain(
            json!({"ok":true,"path":path,"replacements":found}),
        ))
    }

    fn apply_patch(&self, args: &Value) -> Result<ToolResult> {
        let patch = required_str(args, "patch")?;
        let changed = apply_openai_patch(&self.cwd, patch)?;
        Ok(ToolResult::plain(
            json!({"ok":true,"changed_files":changed}),
        ))
    }

    async fn exec_command(&self, args: &Value) -> Result<ToolResult> {
        let command = required_str(args, "command")?.to_string();
        let workdir = args
            .get("workdir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .map(|p| if p.is_absolute() { p } else { self.cwd.join(p) })
            .unwrap_or_else(|| self.cwd.clone());
        let yield_ms = args
            .get("yield_time_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000)
            .clamp(250, 30_000);
        let v = self
            .terminals
            .exec(
                command,
                workdir,
                yield_ms,
                false,
                &self.store,
                &self.agent_id,
                self.preview_bytes,
            )
            .await?;
        Ok(ToolResult::plain(v))
    }

    async fn write_stdin(&self, args: &Value) -> Result<ToolResult> {
        let id = required_str(args, "terminal_id")?;
        let input = args.get("input").and_then(Value::as_str).unwrap_or("");
        let yield_ms = args
            .get("yield_time_ms")
            .and_then(Value::as_u64)
            .unwrap_or(if input.is_empty() { 5000 } else { 250 })
            .clamp(0, 30_000);
        Ok(ToolResult::plain(
            self.terminals
                .write_stdin(id, input, yield_ms, self.preview_bytes)
                .await?,
        ))
    }

    async fn terminate_terminal(&self, args: &Value) -> Result<ToolResult> {
        let id = required_str(args, "terminal_id")?;
        Ok(ToolResult::plain(
            json!({"ok":true,"terminated":self.terminals.terminate(id).await?}),
        ))
    }

    fn read_output(&self, args: &Value) -> Result<ToolResult> {
        let output_ref = required_str(args, "output_ref")?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(MAX_READ_BYTES as u64)
            .clamp(1, MAX_READ_BYTES as u64) as usize;
        let path = self.store.output_path(&self.agent_id, output_ref)?;
        let mut file = fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0; limit];
        let read = file.read(&mut buf)?;
        buf.truncate(read);
        Ok(ToolResult::plain(
            json!({"ok":true,"output_ref":output_ref,"offset":offset,"next_offset":offset+read as u64,"content":String::from_utf8_lossy(&buf),"eof":read<limit}),
        ))
    }

    fn view_image(&self, args: &Value) -> Result<ToolResult> {
        let path = self.path(args)?;
        let data = fs::read(&path)?;
        if data.len() > 5 * 1024 * 1024 {
            bail!("image exceeds 5 MiB");
        }
        let mime = mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string();
        if !mime.starts_with("image/") {
            bail!("not a recognized image file");
        }
        let url = format!("data:{mime};base64,{}", STANDARD.encode(&data));
        Ok(ToolResult {
            content: json!({"ok":true,"path":path,"mime_type":mime,"bytes":data.len(),"note":"image attached in the next model-visible message"}),
            image_message: Some(
                json!({"role":"user","content":[{"type":"text","text":format!("Image from {}",path.display())},{"type":"image_url","image_url":{"url":url}}]}),
            ),
        })
    }
}

fn required_str<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument: {key}"))
}
fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.into();
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    format!("{}…", &s[..n])
}

#[derive(Clone, Default)]
pub struct TerminalManager {
    sessions: Arc<Mutex<HashMap<String, Arc<TerminalSession>>>>,
}

struct TerminalSession {
    id: String,
    command: String,
    cwd: PathBuf,
    pid: i32,
    child: AsyncMutex<Child>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    output_path: PathBuf,
    output_ref: String,
    cursor: Mutex<u64>,
    exit_code: Mutex<Option<i32>>,
    cancelled: AtomicBool,
    cancellation: tokio::sync::Notify,
}

impl TerminalManager {
    #[allow(clippy::too_many_arguments)]
    pub async fn exec(
        &self,
        command: String,
        cwd: PathBuf,
        yield_ms: u64,
        _tty: bool,
        store: &Store,
        agent_id: &str,
        preview_bytes: usize,
    ) -> Result<Value> {
        if self.running_count() >= MAX_TERMINALS {
            bail!("background terminal limit reached ({MAX_TERMINALS})");
        }
        let id = format!("term_{}", ulid::Ulid::new());
        let output_ref = format!("out_{}", ulid::Ulid::new());
        let output_path = store.output_path(agent_id, &output_ref)?;
        let stdout_file = private_output_file(&output_path)?;
        let stderr_file = stdout_file.try_clone()?;
        let mut cmd = Command::new("bash");
        cmd.arg("-lc")
            .arg(&command)
            .current_dir(&cwd)
            .env_remove("OPENAI_API_KEY")
            .env_remove("SUBAGENT_WEB_PASSWORD")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().with_context(|| format!("execute {command}"))?;
        let pid = child.id().context("child has no pid")? as i32;
        let stdin = child.stdin.take();
        let session = Arc::new(TerminalSession {
            id: id.clone(),
            command,
            cwd,
            pid,
            child: AsyncMutex::new(child),
            stdin: AsyncMutex::new(stdin),
            output_path,
            output_ref: output_ref.clone(),
            cursor: Mutex::new(0),
            exit_code: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            cancellation: tokio::sync::Notify::new(),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());
        let waiter_session = session.clone();
        tokio::spawn(async move {
            loop {
                if refresh_exit_code(&waiter_session).await.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        if !session.cancelled.load(Ordering::Acquire) {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(yield_ms)) => {}
                _ = session.cancellation.notified() => {}
            }
        }
        let status = refresh_exit_code(&session).await;
        let cancelled = session.cancelled.load(Ordering::Acquire);
        let output = read_preview(&session.output_path, preview_bytes)?;
        if cancelled {
            self.sessions.lock().unwrap().remove(&id);
            return Ok(json!({
                "ok":false,
                "status":"cancelled",
                "terminal_id":id,
                "exit_code":status,
                "output":output,
                "output_ref":output_ref,
                "truncated":output.truncated,
            }));
        }
        if let Some(exit_code) = status {
            self.sessions.lock().unwrap().remove(&id);
            Ok(
                json!({"ok":exit_code==0,"status":"completed","exit_code":exit_code,"output":output,"output_ref":output_ref,"truncated":output.truncated}),
            )
        } else {
            Ok(
                json!({"ok":true,"status":"running","terminal_id":id,"output":output,"output_ref":output_ref,"truncated":output.truncated}),
            )
        }
    }

    pub async fn write_stdin(
        &self,
        id: &str,
        input: &str,
        yield_ms: u64,
        preview_bytes: usize,
    ) -> Result<Value> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .with_context(|| format!("terminal not found: {id}"))?;
        if !input.is_empty()
            && let Some(stdin) = session.stdin.lock().await.as_mut()
        {
            stdin.write_all(input.as_bytes()).await?;
            stdin.flush().await?;
        }
        tokio::time::sleep(Duration::from_millis(yield_ms)).await;
        let (chunk, next, truncated) = {
            let mut cursor = session.cursor.lock().unwrap();
            let start = *cursor;
            let result = read_chunk(&session.output_path, start, preview_bytes)?;
            *cursor = result.1;
            result
        };
        let exit_code = refresh_exit_code(&session).await;
        if exit_code.is_some() {
            self.sessions.lock().unwrap().remove(id);
        }
        Ok(
            json!({"ok":exit_code.is_none_or(|c|c==0),"terminal_id":id,"status":if exit_code.is_some(){"completed"}else{"running"},"exit_code":exit_code,"output":chunk,"output_ref":session.output_ref,"next_offset":next,"truncated":truncated}),
        )
    }

    pub fn list(&self) -> Value {
        let sessions = self.sessions.lock().unwrap();
        let data: Vec<_> = sessions
            .values()
            .filter(|session| session.exit_code.lock().unwrap().is_none())
            .map(|session| {
                json!({
                    "terminal_id": session.id,
                    "command": session.command,
                    "cwd": session.cwd,
                    "pid": session.pid,
                    "output_ref": session.output_ref,
                })
            })
            .collect();
        json!({"ok":true,"terminals":data,"count":data.len(),"limit":MAX_TERMINALS})
    }

    pub async fn terminate(&self, id: &str) -> Result<bool> {
        let session = self.sessions.lock().unwrap().remove(id);
        if let Some(s) = session {
            s.cancelled.store(true, Ordering::Release);
            s.cancellation.notify_waiters();
            terminate_sessions(&[s]).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn terminate_all(&self) -> Result<()> {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .drain()
            .map(|(_, s)| s)
            .collect();
        for s in &sessions {
            s.cancelled.store(true, Ordering::Release);
            s.cancellation.notify_waiters();
        }
        terminate_sessions(&sessions).await
    }

    fn running_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.exit_code.lock().unwrap().is_none())
            .count()
    }
}

#[derive(serde::Serialize)]
struct Preview {
    content: String,
    head_bytes: usize,
    tail_bytes: usize,
    total_bytes: u64,
    truncated: bool,
}

fn private_output_file(path: &Path) -> Result<fs::File> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    opts.mode(0o600);
    Ok(opts.open(path)?)
}

fn read_preview(path: &Path, max: usize) -> Result<Preview> {
    let mut f = fs::File::open(path)?;
    let total = f.metadata()?.len();
    if total as usize <= max {
        let mut b = Vec::new();
        f.read_to_end(&mut b)?;
        return Ok(Preview {
            content: String::from_utf8_lossy(&b).into(),
            head_bytes: b.len(),
            tail_bytes: 0,
            total_bytes: total,
            truncated: false,
        });
    }
    let head = max * 3 / 4;
    let tail = max - head;
    let mut hb = vec![0; head];
    f.read_exact(&mut hb)?;
    f.seek(SeekFrom::End(-(tail as i64)))?;
    let mut tb = vec![0; tail];
    f.read_exact(&mut tb)?;
    Ok(Preview {
        content: format!(
            "{}\n… {} bytes omitted …\n{}",
            String::from_utf8_lossy(&hb),
            total - max as u64,
            String::from_utf8_lossy(&tb)
        ),
        head_bytes: head,
        tail_bytes: tail,
        total_bytes: total,
        truncated: true,
    })
}

fn read_chunk(path: &Path, offset: u64, max: usize) -> Result<(String, u64, bool)> {
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut b = vec![0; max];
    let n = f.read(&mut b)?;
    b.truncate(n);
    let next = offset + n as u64;
    let truncated = next < f.metadata()?.len();
    Ok((String::from_utf8_lossy(&b).into(), next, truncated))
}

async fn refresh_exit_code(session: &TerminalSession) -> Option<i32> {
    if let Some(code) = *session.exit_code.lock().unwrap() {
        return Some(code);
    }
    let status = session.child.lock().await.try_wait().ok().flatten();
    if let Some(status) = status {
        let code = status.code().unwrap_or(-1);
        *session.exit_code.lock().unwrap() = Some(code);
        Some(code)
    } else {
        None
    }
}

async fn signal_if_running(session: &TerminalSession, signal: i32) -> Result<bool> {
    let mut child = session.child.lock().await;
    if let Some(status) = child.try_wait().ok().flatten() {
        *session.exit_code.lock().unwrap() = Some(status.code().unwrap_or(-1));
        return Ok(false);
    }
    // The child cannot be reaped or have its PID reused while the Child lock is held.
    let result = unsafe { libc::kill(-session.pid, signal) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(true)
}

async fn terminate_sessions(sessions: &[Arc<TerminalSession>]) -> Result<()> {
    let mut signalled = false;
    for session in sessions {
        signalled |= signal_if_running(session, libc::SIGTERM).await?;
    }
    if !signalled {
        return Ok(());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    for session in sessions {
        signal_if_running(session, libc::SIGKILL).await?;
        let mut child = session.child.lock().await;
        let status = match child.try_wait()? {
            Some(status) => status,
            None => child.wait().await?,
        };
        *session.exit_code.lock().unwrap() = Some(status.code().unwrap_or(-1));
    }
    Ok(())
}

fn apply_openai_patch(root: &Path, patch: &str) -> Result<Vec<String>> {
    let mut lines = patch.lines().peekable();
    if lines.next() != Some("*** Begin Patch") {
        bail!("patch must start with *** Begin Patch");
    }
    let mut changed = Vec::new();
    while let Some(line) = lines.next() {
        if line == "*** End Patch" {
            return Ok(changed);
        }
        if let Some(rel) = line.strip_prefix("*** Add File: ") {
            let mut content = String::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") {
                    break;
                }
                let l = lines.next().unwrap();
                content.push_str(l.strip_prefix('+').unwrap_or(l));
                content.push('\n');
            }
            let path = root.join(rel);
            if let Some(p) = path.parent() {
                fs::create_dir_all(p)?;
            }
            fs::write(path, content)?;
            changed.push(rel.into());
            continue;
        }
        if let Some(rel) = line.strip_prefix("*** Delete File: ") {
            fs::remove_file(root.join(rel))?;
            changed.push(rel.into());
            continue;
        }
        if let Some(rel) = line.strip_prefix("*** Update File: ") {
            let path = root.join(rel);
            let mut body = fs::read_to_string(&path)?;
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") {
                    break;
                }
                if !next.starts_with("@@") {
                    bail!("expected @@ hunk in update for {rel}");
                }
                lines.next();
                let mut old = String::new();
                let mut new = String::new();
                while let Some(h) = lines.peek() {
                    if h.starts_with("@@") || h.starts_with("*** ") {
                        break;
                    }
                    let h = lines.next().unwrap();
                    match h.chars().next() {
                        Some(' ') => {
                            old.push_str(&h[1..]);
                            old.push('\n');
                            new.push_str(&h[1..]);
                            new.push('\n');
                        }
                        Some('-') => {
                            old.push_str(&h[1..]);
                            old.push('\n');
                        }
                        Some('+') => {
                            new.push_str(&h[1..]);
                            new.push('\n');
                        }
                        _ => bail!("invalid patch hunk line"),
                    }
                }
                let count = body.matches(&old).count();
                if count != 1 {
                    bail!("update hunk for {rel} matched {count} locations")
                };
                body = body.replacen(&old, &new, 1);
            }
            fs::write(path, body)?;
            changed.push(rel.into());
            continue;
        }
        bail!("invalid patch directive: {line}");
    }
    bail!("patch missing *** End Patch")
}

pub fn tool_definitions(mode: AgentMode) -> Vec<Value> {
    let mut defs = vec![
        tool(
            "read",
            "Read a UTF-8 file with 1-based line numbers. Use bounded offset and limit.",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":2000}},"required":["path"],"additionalProperties":false}),
        ),
        tool(
            "glob",
            "Find files and directories matching a glob pattern.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"limit":{"type":"integer"}},"required":["pattern"],"additionalProperties":false}),
        ),
        tool(
            "grep",
            "Search UTF-8 files using a regular expression.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string"},"limit":{"type":"integer"}},"required":["pattern"],"additionalProperties":false}),
        ),
        tool(
            "exec_command",
            "Run a Bash command. It returns a terminal_id if still running after yield-time-ms.",
            json!({"type":"object","properties":{"command":{"type":"string"},"workdir":{"type":"string"},"yield_time_ms":{"type":"integer","minimum":250,"maximum":30000}},"required":["command"],"additionalProperties":false}),
        ),
        tool(
            "write_stdin",
            "Write to or poll a live terminal.",
            json!({"type":"object","properties":{"terminal_id":{"type":"string"},"input":{"type":"string"},"yield_time_ms":{"type":"integer","minimum":0,"maximum":30000}},"required":["terminal_id"],"additionalProperties":false}),
        ),
        tool(
            "list_terminals",
            "List live background terminals for this agent.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "terminate_terminal",
            "Terminate one background terminal and its process group.",
            json!({"type":"object","properties":{"terminal_id":{"type":"string"}},"required":["terminal_id"],"additionalProperties":false}),
        ),
        tool(
            "terminate_all_terminals",
            "Terminate every background terminal owned by this agent.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "view_image",
            "Attach a local image to the next model-visible message.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
        ),
        tool(
            "read_output",
            "Read a bounded byte range from stored command output.",
            json!({"type":"object","properties":{"output_ref":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":65536}},"required":["output_ref"],"additionalProperties":false}),
        ),
        tool(
            "notify",
            "Publish a concise progress, milestone, input-required, or blocked update to the master's durable inbox.",
            json!({"type":"object","properties":{"event_type":{"type":"string","enum":["progress","milestone","input_required","blocked"]},"summary":{"type":"string","minLength":1,"maxLength":5000}},"required":["event_type","summary"],"additionalProperties":false}),
        ),
    ];
    if mode == AgentMode::Write {
        defs.extend([
        tool("write","Create or replace a complete file.",json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false})),
        tool("edit","Replace exact text in an existing file.",json!({"type":"object","properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"},"expected_replacements":{"type":"integer","minimum":1}},"required":["path","old_text","new_text"],"additionalProperties":false})),
        tool("apply_patch","Apply an OpenAI-style Begin Patch/Add File/Update File/Delete File patch.",json!({"type":"object","properties":{"patch":{"type":"string"}},"required":["patch"],"additionalProperties":false})),
    ]);
    }
    defs
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn runtime(temp: &tempfile::TempDir) -> ToolRuntime {
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
            runtime_dir: temp.path().join("run"),
        };
        let store = Store::new(&paths).unwrap();
        crate::config::ensure_private_dir(&store.outputs_dir("agt_test")).unwrap();
        ToolRuntime {
            agent_id: "agt_test".into(),
            cwd: temp.path().into(),
            mode: AgentMode::Write,
            store,
            terminals: TerminalManager::default(),
            preview_bytes: 1024,
        }
    }

    #[test]
    fn patch_add_edit_delete_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let added = apply_openai_patch(
            temp.path(),
            "*** Begin Patch\n*** Add File: a.txt\n+alpha\n*** End Patch",
        )
        .unwrap();
        assert_eq!(added, vec!["a.txt"]);
        apply_openai_patch(
            temp.path(),
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-alpha\n+beta\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "beta\n"
        );
        apply_openai_patch(
            temp.path(),
            "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch",
        )
        .unwrap();
        assert!(!temp.path().join("a.txt").exists());
    }

    #[test]
    fn readonly_omits_all_structured_writers() {
        let names = tool_definitions(AgentMode::Readonly)
            .into_iter()
            .map(|v| v["function"]["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(names.contains(&"read".to_string()));
        assert!(!names.contains(&"write".to_string()));
        assert!(!names.contains(&"edit".to_string()));
        assert!(!names.contains(&"apply_patch".to_string()));
    }

    #[test]
    fn write_mode_offers_all_editing_styles() {
        let names = tool_definitions(AgentMode::Write)
            .into_iter()
            .map(|v| v["function"]["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        for name in ["write", "edit", "apply_patch"] {
            assert!(names.contains(&name.to_string()));
        }
    }

    #[tokio::test]
    async fn glob_of_empty_directory_returns_no_empty_path() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        fs::create_dir(temp.path().join("empty")).unwrap();
        let result = runtime
            .execute_inner("glob", r#"{"path":"empty","pattern":"*"}"#)
            .await
            .unwrap();
        assert_eq!(result.content["paths"], json!([]));
    }

    #[tokio::test]
    async fn image_and_stored_output_are_model_accessible() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        fs::write(temp.path().join("pixel.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let image = runtime
            .execute_inner("view_image", r#"{"path":"pixel.png"}"#)
            .await
            .unwrap();
        assert!(image.image_message.is_some());

        fs::write(
            runtime.store.output_path("agt_test", "out_test").unwrap(),
            "0123456789",
        )
        .unwrap();
        let output = runtime
            .execute_inner(
                "read_output",
                r#"{"output_ref":"out_test","offset":2,"limit":4}"#,
            )
            .await
            .unwrap();
        assert_eq!(output.content["content"], "2345");
    }

    #[tokio::test]
    async fn cancelling_during_exec_yield_returns_without_panic() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let terminals = runtime.terminals.clone();
        let store = runtime.store.clone();
        let cwd = runtime.cwd.clone();
        let task = tokio::spawn(async move {
            terminals
                .exec(
                    "sleep 30".into(),
                    cwd,
                    30_000,
                    false,
                    &store,
                    "agt_test",
                    1024,
                )
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        runtime.terminals.terminate_all().await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cancelled exec should wake before yield timeout")
            .unwrap();
        assert_eq!(result["status"], "cancelled");
        assert_eq!(result["ok"], false);
        assert_eq!(runtime.terminals.list()["count"], 0);
    }
}
