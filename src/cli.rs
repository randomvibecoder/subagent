use crate::{
    config::{FileConfig, Paths, RuntimeConfig, ensure_private_dir},
    daemon,
    ipc::{AgentMode, ListFilter, Request, error_json},
};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

#[derive(Parser)]
#[command(
    name="subagent",
    version,
    color=clap::ColorChoice::Never,
    about="JSONL-only background coding-agent manager",
    long_about=None,
    after_help="All command data is JSONL. Configure the daemon with OPENAI_API_KEY, OPENAI_BASE_URL, and OPENAI_MODEL. No human/table output mode is provided. Agent shell commands are unsandboxed; readonly mode is advisory."
)]
struct Cli {
    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Subcommand)]
enum TopCommand {
    #[command(
        subcommand,
        about = "Manage the per-user daemon. Output: one daemon JSON object."
    )]
    Daemon(DaemonCommand),
    #[command(
        subcommand,
        about = "Manage persisted agents. Output: one JSON object per line."
    )]
    Agents(AgentsCommand),
    #[command(subcommand, about = "Manage non-secret configuration. Output: JSONL.")]
    Config(ConfigCommand),
    #[command(name = "__serve", hide = true)]
    Serve,
}

#[derive(Subcommand)]
enum DaemonCommand {
    Start,
    Status,
    Stop,
}
#[derive(Subcommand)]
enum ConfigCommand {
    List,
    Get { key: String },
    Set { key: String, value: String },
}

#[derive(Subcommand)]
enum AgentsCommand {
    #[command(
        about = "Spawn an agent. Output schema: {type,id,title,dir,mode,status,spawned_at,deadline_at,...}"
    )]
    Spawn(SpawnArgs),
    #[command(about = "List agents, newest first. Each JSONL line is one agent object.")]
    List(ListArgs),
    #[command(about = "Get one agent metadata object.")]
    Status { id: String },
    #[command(about = "Read event JSONL. Default: last 100 events.")]
    Logs(LogsArgs),
    #[command(
        about = "Read LLM-sized context JSONL. Default: user_message and assistant_message only."
    )]
    Context(ContextArgs),
    #[command(about = "Send input at the next safe boundary, or resume a non-working agent.")]
    Send(SendArgs),
    #[command(
        visible_alias = "btw",
        about = "Answer a question with a readonly side agent over a snapshot of the parent context."
    )]
    Side(SideArgs),
    #[command(about = "Set a working agent deadline to HOURS from now (0 < HOURS <= 100).")]
    Time { id: String, hours: f64 },
    #[command(about = "Stop a working agent and all its terminal process groups.")]
    Stop { id: String },
    #[command(about = "Permanently delete a non-working agent history.")]
    Delete { id: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Readonly,
    Write,
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Working,
    Finished,
    Stopped,
    Failed,
}

impl StatusArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Finished => "finished",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}
impl From<ModeArg> for AgentMode {
    fn from(v: ModeArg) -> Self {
        match v {
            ModeArg::Readonly => Self::Readonly,
            ModeArg::Write => Self::Write,
        }
    }
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="JSONL output: {type:\"agent\",id,title,dir,mode,advisory_readonly,model,status,spawned_at,run_started_at,updated_at,finished_at,stopped_at,failed_at,deadline_at,run_number,stop_reason,last_error}"
)]
struct SpawnArgs {
    /// Existing working directory for the agent.
    #[arg(long)]
    dir: String,
    /// Inline task text. Conflicts with --message-file.
    #[arg(long)]
    message: Option<String>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Read UTF-8 task input from PATH; use - for stdin"
    )]
    message_file: Option<String>,
    /// Stable display title; defaults to the first non-empty task line.
    #[arg(long)]
    title: Option<String>,
    /// readonly omits structured write tools but Bash remains advisory.
    #[arg(long, value_enum, default_value = "readonly")]
    mode: ModeArg,
    /// Optional deadline in hours from spawn; must be >0 and <=100.
    #[arg(long, value_name = "HOURS")]
    wall_time: Option<f64>,
}

#[derive(Args)]
#[command(
    after_help = "Each JSONL line has the same agent schema returned by spawn. No line is emitted when no agents match."
)]
struct ListArgs {
    /// Filter by status; repeat for working, finished, stopped, or failed.
    #[arg(long = "status")]
    statuses: Vec<StatusArg>,
    /// Filter by canonical working directory.
    #[arg(long)]
    dir: Option<String>,
    /// Include agents spawned at or after this RFC3339 timestamp.
    #[arg(long)]
    spawned_after: Option<String>,
    /// Include agents spawned at or before this RFC3339 timestamp.
    #[arg(long)]
    spawned_before: Option<String>,
    /// Include agents finished at or after this RFC3339 timestamp.
    #[arg(long)]
    finished_after: Option<String>,
    /// Include agents finished at or before this RFC3339 timestamp.
    #[arg(long)]
    finished_before: Option<String>,
    /// Metadata timestamp used for ordering.
    #[arg(long,default_value="spawned_at",value_parser=["spawned_at","updated_at","finished_at"])]
    sort: String,
    /// Sort direction.
    #[arg(long,default_value="desc",value_parser=["asc","desc"])]
    order: String,
    /// Maximum emitted agent objects.
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Number of matching objects to skip.
    #[arg(long, default_value_t = 0)]
    offset: usize,
}

#[derive(Args)]
#[command(
    after_help = "Event JSONL schema: {event_id,agent_id,sequence,timestamp,type,data}. Types include lifecycle,user_message,assistant_message,reasoning,tool_call,tool_result,error."
)]
struct LogsArgs {
    /// Agent ID.
    id: String,
    /// Event type to include; repeatable. Empty means every event type.
    #[arg(long = "type")]
    types: Vec<String>,
    /// Emit only events after this event ID.
    #[arg(long)]
    after: Option<String>,
    /// Maximum historical events; defaults to the newest 100.
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Keep the connection open and stream newly appended events.
    #[arg(long)]
    follow: bool,
}
#[derive(Args)]
#[command(
    after_help = "First line: {type:\"context_meta\",agent_id,estimated_tokens,max_tokens,truncated,included_types}. Remaining lines use the event schema."
)]
struct ContextArgs {
    /// Agent ID.
    id: String,
    /// Event type to include; repeatable. Defaults to user_message and assistant_message.
    #[arg(long)]
    include: Vec<String>,
    /// Approximate maximum output tokens. The original user task is preserved when possible.
    #[arg(long, default_value_t = 12_000)]
    max_tokens: usize,
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="If status is working, input is queued for the next safe model boundary. Otherwise this starts a new run. Output: one updated agent object."
)]
struct SendArgs {
    /// Agent ID.
    id: String,
    /// Inline user message. Conflicts with --message-file.
    #[arg(long)]
    message: Option<String>,
    /// Read UTF-8 input from PATH; use - for stdin.
    #[arg(long, value_name = "PATH")]
    message_file: Option<String>,
    /// Optional new-run deadline, or reset the active deadline from now.
    #[arg(long, value_name = "HOURS")]
    wall_time: Option<f64>,
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="The side agent inherits a snapshot of the parent's full model context and workspace. It can read, search, run non-mutating Bash, poll terminals, read output, and view images, but never receives write, edit, or patch tools. It runs independently and does not append its question, tool calls, or answer to the parent transcript. Output: {type:\"side_answer\",side_id,agent_id,answer,model,mode,parent_mode,ephemeral,inherited_context_messages,tool_calls}."
)]
struct SideArgs {
    /// Parent agent ID.
    id: String,
    /// Inline side question. Conflicts with --message-file.
    #[arg(long)]
    message: Option<String>,
    /// Read UTF-8 input from PATH; use - for stdin.
    #[arg(long, value_name = "PATH")]
    message_file: Option<String>,
    /// Optional side-agent deadline in hours; must be >0 and <=100.
    #[arg(long, value_name = "HOURS")]
    wall_time: Option<f64>,
}

pub async fn run() -> Result<()> {
    if let Err(e) = run_inner().await {
        eprintln!(
            "{}",
            serde_json::to_string(&error_json("cli_error", format!("{e:#}")))?
        );
        std::process::exit(2)
    }
    Ok(())
}

async fn run_inner() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            print!("{error}");
            return Ok(());
        }
        Err(error) => bail!(error.to_string()),
    };
    match cli.command {
        TopCommand::Serve => daemon::serve(RuntimeConfig::load()?).await,
        TopCommand::Daemon(DaemonCommand::Start) => start_daemon().await,
        TopCommand::Daemon(DaemonCommand::Status) => request(Request::DaemonStatus).await,
        TopCommand::Daemon(DaemonCommand::Stop) => request(Request::DaemonStop).await,
        TopCommand::Config(command) => config_command(command),
        TopCommand::Agents(command) => {
            let req = match command {
                AgentsCommand::Spawn(a) => Request::AgentSpawn {
                    dir: a.dir,
                    message: read_message(a.message, a.message_file).await?,
                    title: a.title,
                    mode: a.mode.into(),
                    wall_time_hours: a.wall_time,
                },
                AgentsCommand::List(a) => Request::AgentList {
                    filter: ListFilter {
                        statuses: a
                            .statuses
                            .into_iter()
                            .map(|status| status.as_str().to_string())
                            .collect(),
                        dir: a.dir,
                        spawned_after: a.spawned_after,
                        spawned_before: a.spawned_before,
                        finished_after: a.finished_after,
                        finished_before: a.finished_before,
                        sort: a.sort,
                        order: a.order,
                        limit: a.limit,
                        offset: a.offset,
                    },
                },
                AgentsCommand::Status { id } => Request::AgentStatus { id },
                AgentsCommand::Logs(a) => Request::AgentLogs {
                    id: a.id,
                    types: a.types,
                    after: a.after,
                    limit: a.limit,
                    follow: a.follow,
                },
                AgentsCommand::Context(a) => Request::AgentContext {
                    id: a.id,
                    include: a.include,
                    max_tokens: a.max_tokens,
                },
                AgentsCommand::Send(a) => Request::AgentSend {
                    id: a.id,
                    message: read_message(a.message, a.message_file).await?,
                    wall_time_hours: a.wall_time,
                },
                AgentsCommand::Side(a) => Request::AgentSide {
                    id: a.id,
                    message: read_message(a.message, a.message_file).await?,
                    wall_time_hours: a.wall_time,
                },
                AgentsCommand::Time { id, hours } => Request::AgentTime { id, hours },
                AgentsCommand::Stop { id } => Request::AgentStop { id },
                AgentsCommand::Delete { id } => Request::AgentDelete { id },
            };
            request(req).await
        }
    }
}

fn config_command(command: ConfigCommand) -> Result<()> {
    let paths = Paths::discover()?;
    let mut cfg = FileConfig::load(&paths)?;
    match command {
        ConfigCommand::List => println!(
            "{}",
            serde_json::to_string(
                &json!({"type":"config","base-url":cfg.base_url,"model":cfg.model,"max-agents":cfg.max_agents,"context-token-budget":cfg.context_token_budget,"tool-output-preview-bytes":cfg.tool_output_preview_bytes})
            )?
        ),
        ConfigCommand::Get { key } => println!(
            "{}",
            serde_json::to_string(
                &json!({"type":"config_value","key":key,"value":cfg.get(&key)?})
            )?
        ),
        ConfigCommand::Set { key, value } => {
            cfg.set(&key, &value)?;
            cfg.save(&paths)?;
            println!(
                "{}",
                serde_json::to_string(
                    &json!({"type":"config_value","key":key,"value":cfg.get(&key)?,"note":"restart daemon for this value to take effect"})
                )?
            );
        }
    }
    Ok(())
}

async fn start_daemon() -> Result<()> {
    let cfg = RuntimeConfig::load()?;
    let socket = cfg.paths.socket();
    if UnixStream::connect(&socket).await.is_ok() {
        bail!("daemon is already running")
    }
    ensure_private_dir(&cfg.paths.state_dir)?;
    ensure_private_dir(&cfg.paths.runtime_dir)?;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut log_options = fs::OpenOptions::new();
    log_options.create(true).append(true);
    #[cfg(unix)]
    log_options.mode(0o600);
    let log = log_options.open(cfg.paths.daemon_log())?;
    let err = log.try_clone()?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("start daemon")?;
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            return request(Request::DaemonStatus).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!(
        "daemon process {} did not become ready; inspect {}",
        child.id(),
        cfg.paths.daemon_log().display()
    )
}

async fn request(req: Request) -> Result<()> {
    let paths = Paths::discover()?;
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .context("daemon is not running; run 'subagent daemon start'")?;
    let mut body = serde_json::to_vec(&req)?;
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    let mut lines = BufReader::new(stream).lines();
    let mut failed = false;
    while let Some(line) = lines.next_line().await? {
        let value: Value = serde_json::from_str(&line)?;
        if value.get("type").and_then(Value::as_str) == Some("error") {
            eprintln!("{line}");
            failed = true
        } else {
            println!("{line}")
        }
    }
    if failed {
        std::process::exit(4)
    }
    Ok(())
}

async fn read_message(inline: Option<String>, file: Option<String>) -> Result<String> {
    if let Some(m) = inline {
        return Ok(m);
    }
    let path = file.context("message or message-file is required")?;
    if path == "-" {
        let mut s = String::new();
        let mut stdin = BufReader::new(tokio::io::stdin());
        use tokio::io::AsyncReadExt;
        stdin.read_to_string(&mut s).await?;
        Ok(s)
    } else {
        fs::read_to_string(&path).with_context(|| format!("read message file {path}"))
    }
}
