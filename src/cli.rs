use crate::{
    config::{FileConfig, Paths, RuntimeConfig, ensure_private_dir},
    daemon,
    ipc::{AgentMode, ListFilter, Request, coded_error, error_json_for},
    store::{canonical_dir, canonical_filter_dir},
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
    #[command(
        subcommand,
        about = "Create and inspect durable readonly Side runs. Output: JSONL."
    )]
    Sides(SidesCommand),
    #[command(
        subcommand,
        about = "Inspect and cancel durable agent messages. Output: JSONL."
    )]
    Messages(MessagesCommand),
    #[command(about = "Read the durable high-signal notification journal. Output: JSONL.")]
    Inbox(InboxArgs),
    #[command(subcommand, about = "Manage non-secret configuration. Output: JSONL.")]
    Config(ConfigCommand),
    #[command(name = "__serve", hide = true)]
    Serve {
        #[arg(long)]
        web_ui_port: Option<u16>,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    #[command(about = "Start the detached daemon, optionally with a localhost Web UI.")]
    Start {
        /// Bind the embedded human dashboard to 127.0.0.1:PORT.
        #[arg(long, value_name = "PORT", value_parser = parse_port)]
        web_ui_port: Option<u16>,
    },
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
enum MessagesCommand {
    #[command(about = "List durable messages for an agent.")]
    List {
        agent_id: String,
        #[arg(long = "status", value_parser = ["pending", "delivered", "cancelled"])]
        statuses: Vec<String>,
    },
    #[command(about = "Get one durable message.")]
    Status {
        agent_id: String,
        message_id: String,
    },
    #[command(about = "Cancel one pending message.")]
    Cancel {
        agent_id: String,
        message_id: String,
    },
}

#[derive(Subcommand)]
enum SidesCommand {
    #[command(about = "Start a one-shot Side run and return its side_id immediately.")]
    Create(SideArgs),
    #[command(about = "List persisted Side runs for one parent Agent.")]
    List {
        agent_id: String,
        #[arg(long = "status")]
        statuses: Vec<StatusArg>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    #[command(about = "Get complete Side metadata.")]
    Status { id: String },
    #[command(about = "Read Side Event JSONL.")]
    Logs(LogsArgs),
    #[command(about = "Stop a working Side run.")]
    Stop { id: String },
    #[command(about = "Delete a terminal Side history.")]
    Delete { id: String },
}

#[derive(Subcommand)]
enum AgentsCommand {
    #[command(about = "Spawn a named agent and return its ID immediately.")]
    Spawn(SpawnArgs),
    #[command(about = "List agents, newest first. Each JSONL line is one compact list item.")]
    List(ListArgs),
    #[command(about = "Get one agent metadata object.")]
    Status { id: String },
    #[command(about = "Rename an agent tracking label.")]
    Rename {
        /// Stable Agent ID.
        id: String,
        /// New unique display name (4 through 40 characters).
        name: String,
    },
    #[command(about = "Read transcript Event JSONL. Default: last 20 message events.")]
    Logs(LogsArgs),
    #[command(about = "Dump the complete current raw model context as JSONL for debugging.")]
    Context(ContextArgs),
    #[command(about = "Send input at the next safe boundary, or resume a non-working agent.")]
    Send(SendArgs),
    #[command(
        visible_alias = "btw",
        about = "Answer a question with a readonly side agent over a snapshot of the parent context."
    )]
    Side(SideArgs),
    #[command(about = "Set a working agent deadline to MINUTES from now (1..=6000).")]
    Time {
        id: String,
        #[arg(value_parser = parse_minutes)]
        minutes: u64,
    },
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
    after_help="JSONL output: one Agent object with a stable agt_<ULID>. The tracking name appears in agents list and rename receipts."
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
    /// Required unique tracking name, trimmed to 4 through 40 characters.
    #[arg(long)]
    name: String,
    /// readonly omits structured write tools but Bash remains advisory.
    #[arg(long, value_enum, default_value = "readonly")]
    mode: ModeArg,
    /// Override the daemon's default model for this agent.
    #[arg(long)]
    model: Option<String>,
    /// Optional deadline in integer minutes from spawn; 1 through 6000.
    #[arg(long, value_name = "MINUTES", value_parser = parse_minutes)]
    wall_time_minutes: Option<u64>,
}

#[derive(Args)]
#[command(
    after_help = "Each JSONL line is {type,id,name,status,dir,mode,spawned_at,last_message_at,updated_at,run_number,working_sides}. No line is emitted when no agents match."
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
    after_help = "Event JSONL schema: {event_id,agent_id,sequence,timestamp,type,data}. Types: system_message,user_message,assistant_message,reasoning,tool_call,tool_result,lifecycle,error."
)]
struct LogsArgs {
    /// Agent ID.
    id: String,
    /// Exact Event type to include; repeatable. Empty selects system/user/assistant messages.
    #[arg(long = "type", value_name = "EVENT_TYPE", value_parser = ["system_message", "user_message", "assistant_message", "reasoning", "tool_call", "tool_result", "lifecycle", "error"])]
    types: Vec<String>,
    /// Include every Event type. Conflicts with --type.
    #[arg(long, conflicts_with = "types")]
    all: bool,
    /// Emit only events after this event ID.
    #[arg(long)]
    after: Option<String>,
    /// Maximum matching historical Events; select newest N and emit chronologically.
    #[arg(long, default_value_t = 20, value_parser = parse_log_limit)]
    limit: usize,
    /// Keep the connection open and stream newly appended events.
    #[arg(long)]
    follow: bool,
}
#[derive(Args)]
#[command(
    after_help = "First line is context_meta; remaining lines are raw model message objects. Redirect to a file or filter narrowly with jq."
)]
struct ContextArgs {
    /// Agent ID.
    id: String,
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="Durably store the message and return one message_sent receipt immediately. Delivery continues in the daemon."
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
    #[arg(long, value_name = "MINUTES", value_parser = parse_minutes)]
    wall_time_minutes: Option<u64>,
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="Start a durable one-shot readonly Side run and return side_created immediately. Inspect progress with sides status or sides logs. The Side trace never enters the parent transcript."
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
    /// Override the parent agent's model for this Side run.
    #[arg(long)]
    model: Option<String>,
    /// Optional side-agent deadline in integer minutes; 1 through 6000.
    #[arg(long, value_name = "MINUTES", value_parser = parse_minutes)]
    wall_time_minutes: Option<u64>,
}

#[derive(Args)]
#[command(
    after_help = "Each line is one notification, newest first. --priority N includes priority N and higher."
)]
struct InboxArgs {
    /// Maximum notifications to emit, from 1 through 100.
    #[arg(long, default_value_t = 20, value_parser = parse_inbox_limit)]
    limit: usize,
    /// Number of matching notifications to skip.
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Minimum priority to include, from 1 through 5.
    #[arg(long, default_value_t = 2, value_parser = parse_priority)]
    priority: u8,
    /// Include notifications for only this main agent ID.
    #[arg(long, value_name = "AGENT_ID")]
    agent: Option<String>,
}

pub async fn run() -> Result<()> {
    if let Err(e) = run_inner().await {
        eprintln!(
            "{}",
            serde_json::to_string(&error_json_for(&e, "cli_error"))?
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
        TopCommand::Serve { web_ui_port } => {
            daemon::serve(RuntimeConfig::load()?, web_ui_port).await
        }
        TopCommand::Daemon(DaemonCommand::Start { web_ui_port }) => start_daemon(web_ui_port).await,
        TopCommand::Daemon(DaemonCommand::Status) => request(Request::DaemonStatus).await,
        TopCommand::Daemon(DaemonCommand::Stop) => request(Request::DaemonStop).await,
        TopCommand::Config(command) => config_command(command),
        TopCommand::Inbox(args) => {
            request(Request::Inbox {
                limit: args.limit,
                offset: args.offset,
                minimum_priority: args.priority,
                agent_id: args.agent,
            })
            .await
        }
        TopCommand::Messages(command) => {
            let req = match command {
                MessagesCommand::List { agent_id, statuses } => {
                    Request::MessageList { agent_id, statuses }
                }
                MessagesCommand::Status {
                    agent_id,
                    message_id,
                } => Request::MessageStatus {
                    agent_id,
                    message_id,
                },
                MessagesCommand::Cancel {
                    agent_id,
                    message_id,
                } => Request::MessageCancel {
                    agent_id,
                    message_id,
                },
            };
            request(req).await
        }
        TopCommand::Sides(command) => {
            let req = match command {
                SidesCommand::Create(args) => Request::AgentSide {
                    id: args.id,
                    message: read_message(args.message, args.message_file).await?,
                    model: args.model,
                    wall_time_minutes: args.wall_time_minutes,
                },
                SidesCommand::List {
                    agent_id,
                    statuses,
                    limit,
                    offset,
                } => Request::SideList {
                    agent_id,
                    statuses: statuses
                        .into_iter()
                        .map(|status| status.as_str().into())
                        .collect(),
                    limit,
                    offset,
                },
                SidesCommand::Status { id } => Request::SideStatus { id },
                SidesCommand::Logs(args) => Request::SideLogs {
                    id: args.id,
                    types: args.types,
                    all: args.all,
                    after: args.after,
                    limit: args.limit,
                    follow: args.follow,
                },
                SidesCommand::Stop { id } => Request::SideStop { id },
                SidesCommand::Delete { id } => Request::SideDelete { id },
            };
            request(req).await
        }
        TopCommand::Agents(command) => {
            let req = match command {
                AgentsCommand::Spawn(a) => Request::AgentSpawn {
                    dir: canonical_dir(&a.dir)?,
                    message: read_message(a.message, a.message_file).await?,
                    name: a.name,
                    mode: a.mode.into(),
                    model: a.model,
                    wall_time_minutes: a.wall_time_minutes,
                },
                AgentsCommand::List(a) => Request::AgentList {
                    filter: ListFilter {
                        statuses: a
                            .statuses
                            .into_iter()
                            .map(|status| status.as_str().to_string())
                            .collect(),
                        dir: a.dir.as_deref().map(canonical_filter_dir).transpose()?,
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
                AgentsCommand::Rename { id, name } => Request::AgentRename { id, name },
                AgentsCommand::Logs(a) => Request::AgentLogs {
                    id: a.id,
                    types: a.types,
                    all: a.all,
                    after: a.after,
                    limit: a.limit,
                    follow: a.follow,
                },
                AgentsCommand::Context(a) => Request::AgentContext { id: a.id },
                AgentsCommand::Send(a) => Request::AgentSend {
                    id: a.id,
                    message: read_message(a.message, a.message_file).await?,
                    wall_time_minutes: a.wall_time_minutes,
                },
                AgentsCommand::Side(a) => Request::AgentSide {
                    id: a.id,
                    message: read_message(a.message, a.message_file).await?,
                    model: a.model,
                    wall_time_minutes: a.wall_time_minutes,
                },
                AgentsCommand::Time { id, minutes } => Request::AgentTime { id, minutes },
                AgentsCommand::Stop { id } => Request::AgentStop { id },
                AgentsCommand::Delete { id } => Request::AgentDelete { id },
            };
            request(req).await
        }
    }
}

fn config_command(command: ConfigCommand) -> Result<()> {
    let paths = Paths::discover()?;
    let mut cfg = if matches!(&command, ConfigCommand::Set { .. }) {
        FileConfig::load_persisted(&paths)?
    } else {
        FileConfig::load(&paths)?
    };
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
            cfg.set(&key, &value).map_err(|error| {
                coded_error(
                    "invalid_argument",
                    format!("{error:#}"),
                    json!({"key":key}),
                    false,
                )
            })?;
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

async fn start_daemon(web_ui_port: Option<u16>) -> Result<()> {
    let cfg = RuntimeConfig::load()?;
    let socket = cfg.paths.socket();
    if UnixStream::connect(&socket).await.is_ok() {
        return Err(coded_error(
            "daemon_already_running",
            "daemon is already running",
            json!({"socket":socket}),
            false,
        ));
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
    cmd.arg("__serve");
    if let Some(port) = web_ui_port {
        cmd.arg("--web-ui-port").arg(port.to_string());
    }
    cmd.stdin(Stdio::null())
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
    let mut stream = UnixStream::connect(paths.socket()).await.map_err(|_| {
        coded_error(
            "daemon_unavailable",
            "daemon is not running; run 'subagent daemon start'",
            json!({"socket":paths.socket()}),
            true,
        )
    })?;
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

fn parse_log_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "limit must be an integer from 1 through 10000".to_string())?;
    if !(1..=10_000).contains(&limit) {
        return Err("limit must be an integer from 1 through 10000".into());
    }
    Ok(limit)
}

fn parse_inbox_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "limit must be an integer from 1 through 100".to_string())?;
    if !(1..=100).contains(&limit) {
        return Err("limit must be an integer from 1 through 100".into());
    }
    Ok(limit)
}

fn parse_priority(value: &str) -> std::result::Result<u8, String> {
    let priority = value
        .parse::<u8>()
        .map_err(|_| "priority must be an integer from 1 through 5".to_string())?;
    if !(1..=5).contains(&priority) {
        return Err("priority must be an integer from 1 through 5".into());
    }
    Ok(priority)
}

fn parse_minutes(value: &str) -> std::result::Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("minutes must be an integer from 1 through 6000".into());
    }
    let minutes = value
        .parse::<u64>()
        .map_err(|_| "minutes must be an integer from 1 through 6000".to_string())?;
    if !(1..=6000).contains(&minutes) {
        return Err("minutes must be an integer from 1 through 6000".into());
    }
    Ok(minutes)
}

fn parse_port(value: &str) -> std::result::Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| "web UI port must be an integer from 1 through 65535".to_string())?;
    if port == 0 {
        return Err("web UI port must be an integer from 1 through 65535".into());
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_use_strict_integer_grammar() {
        for value in ["1", "2", "60", "6000"] {
            assert!(parse_minutes(value).is_ok(), "{value}");
        }
        for value in ["0", ".5", "+1", "1e3", "6001", "NaN"] {
            assert!(parse_minutes(value).is_err(), "{value}");
        }
    }

    #[test]
    fn log_limit_is_positive_and_bounded() {
        assert_eq!(parse_log_limit("1").unwrap(), 1);
        assert_eq!(parse_log_limit("10000").unwrap(), 10_000);
        assert!(parse_log_limit("0").is_err());
        assert!(parse_log_limit("10001").is_err());
    }
}
