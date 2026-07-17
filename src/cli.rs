use crate::{
    config::{
        CONFIG_KEYS, FileConfig, Paths, RuntimeConfig, ensure_private_dir, local_config_values,
        read_daemon_lifecycle,
    },
    daemon,
    ipc::{
        AgentMode, ListFilter, MAX_LIST_LIMIT, PROTOCOL_VERSION, Request, coded_error,
        error_json_for,
    },
    store::{canonical_dir, canonical_filter_dir},
};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const MAX_DAEMON_LOG_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Parser)]
#[command(
    name="subagent",
    version,
    color=clap::ColorChoice::Never,
    about="JSONL-only background coding-agent manager",
    long_about=None,
    after_help="Operational output is always plain JSONL; help and version are plain text. Use `subagent NOUN VERB --help` for argument ranges, output shapes, and examples. Configure the daemon with OPENAI_API_KEY, OPENAI_BASE_URL, and OPENAI_MODEL. No table or non-plain mode is provided. Agent shell commands are unsandboxed; readonly mode is advisory."
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
    #[command(
        subcommand,
        about = "Read the durable high-signal notification journal. Output: JSONL."
    )]
    Inbox(InboxCommand),
    #[command(
        subcommand,
        about = "Inspect the flat Agent and Side team. Output: JSONL."
    )]
    Team(TeamCommand),
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
    #[command(
        about = "Report running daemon configuration and capacity.",
        after_help = "JSONL output: one daemon object with version, protocol_version, PID, socket, capacity, effective model/base URL, and optional Web UI details."
    )]
    Status,
    #[command(
        about = "Request graceful daemon shutdown.",
        after_help = "JSONL output: one daemon object with status stopping and the number of working Agents being stopped. The command returns before process exit."
    )]
    Stop,
}
#[derive(Subcommand)]
enum ConfigCommand {
    #[command(
        about = "List persisted, caller-local, and active daemon configuration.",
        after_help = "JSONL output: one config_value per supported key. active_differs_from_local reports any mismatch; restart_required is true only for an unmasked persisted/default change."
    )]
    List,
    Get {
        /// Configuration key; run `config set --help` for the complete key contract.
        key: String,
    },
    #[command(
        after_help = "Keys:\n  base-url STRING (nonempty; OPENAI_BASE_URL overrides)\n  model STRING (nonempty; OPENAI_MODEL overrides)\n  max-agents INTEGER (0 means unlimited; SUBAGENT_MAX_AGENTS overrides)\n  context-token-budget INTEGER (>0)\n  tool-output-preview-bytes INTEGER (>0)\n  stall-notification-seconds INTEGER (0..=86400; 0 disables; SUBAGENT_STALL_NOTIFICATION_SECONDS overrides)\n\nJSONL output: one config_value. Changes are persisted atomically. Restart only when restart_required is true; active_differs_from_local may be true for harmless environment divergence.\n\nExample:\n  subagent config set model gpt-5.4-mini"
    )]
    Set {
        /// Configuration key listed in the contract below.
        #[arg(value_name = "KEY")]
        key: String,
        /// New persisted value using the selected key's documented format.
        #[arg(value_name = "VALUE")]
        value: String,
    },
}

#[derive(Subcommand)]
enum MessagesCommand {
    #[command(about = "Store a durable message without waking an inactive Agent.")]
    Send(BasicMessageArgs),
    #[command(
        about = "List durable messages newest-first for an Agent.",
        after_help = "Repeated --status filters use OR. JSONL output is zero or more Message records followed by one Agent-scoped list_summary with a nullable next_cursor.\n\nExample:\n  subagent messages list a_7 --status pending --status delivered --limit 100"
    )]
    List {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        agent_id: String,
        #[arg(long = "status", value_parser = ["pending", "delivered", "cancelled"])]
        statuses: Vec<String>,
        /// Maximum Message records to emit, from 1 through 1000.
        #[arg(long, default_value_t = 100, value_parser = parse_list_limit)]
        limit: usize,
        /// Continue toward older Messages from a previous list_summary.
        #[arg(long)]
        after_cursor: Option<String>,
    },
    #[command(about = "Get one durable message.")]
    Status {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        agent_id: String,
        /// Message m_N ref or msg_<ULID>.
        #[arg(value_name = "MESSAGE_ID", value_parser = parse_message_id)]
        message_id: String,
    },
    #[command(about = "Cancel one pending message.")]
    Cancel {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        agent_id: String,
        /// Message m_N ref or msg_<ULID>.
        #[arg(value_name = "MESSAGE_ID", value_parser = parse_message_id)]
        message_id: String,
    },
}

#[derive(Subcommand)]
enum SidesCommand {
    #[command(
        about = "Start a one-shot Side run and return its short ref plus durable Side ID immediately."
    )]
    Create(CreateSideArgs),
    #[command(
        about = "List persisted Side runs for one parent Agent.",
        after_help = "JSONL output: zero or more side_list_item records followed by one list_summary with nullable next_cursor. --after-cursor may be combined only with omitted --offset or --offset 0.\n\nExample:\n  subagent sides list a_7 --status working --limit 20"
    )]
    List {
        /// Parent Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        agent_id: String,
        /// Filter by status; repeat for working, finished, stopped, or failed.
        #[arg(long = "status")]
        statuses: Vec<StatusArg>,
        /// Maximum Side records to emit, from 1 through 1000.
        #[arg(long, default_value_t = 100, value_parser = parse_list_limit)]
        limit: usize,
        /// Number of matching Side records to skip; zero starts at the first match.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Continue toward older Sides from a previous list_summary.
        #[arg(long)]
        after_cursor: Option<String>,
    },
    #[command(about = "Get complete Side metadata.")]
    Status {
        /// Side short ref (s_N) or durable ID (side_<ULID>).
        #[arg(value_name = "SIDE", value_parser = parse_side_id)]
        id: String,
    },
    #[command(about = "Read Side Event JSONL.")]
    Logs(SideLogsArgs),
    #[command(about = "Stop a working Side run.")]
    Stop {
        /// Side short ref (s_N) or durable ID (side_<ULID>).
        #[arg(value_name = "SIDE", value_parser = parse_side_id)]
        id: String,
    },
    #[command(about = "Delete a terminal Side history.")]
    Delete {
        /// Side short ref (s_N) or durable ID (side_<ULID>).
        #[arg(value_name = "SIDE", value_parser = parse_side_id)]
        id: String,
    },
}

#[derive(Subcommand)]
enum AgentsCommand {
    #[command(
        about = "Spawn a named agent and return its preferred short ref plus durable ID immediately."
    )]
    Spawn(SpawnArgs),
    #[command(
        about = "List Agents with configurable sort and order. Each JSONL line is one compact list item."
    )]
    List(ListArgs),
    #[command(about = "Get one agent metadata object.")]
    Status {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        id: String,
    },
    #[command(
        about = "Wait until one Agent reaches a terminal state and emit its final metadata.",
        after_help = "JSONL output: one complete terminal Agent object, or a retryable timeout Error.\n\nExample:\n  subagent agents wait a_7 --timeout-seconds 300"
    )]
    Wait {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        id: String,
        /// Optional bounded wait in seconds, from 1 through 86400; timeout is retryable.
        #[arg(long, value_name = "SECONDS", value_parser = parse_wait_timeout)]
        timeout_seconds: Option<u64>,
    },
    #[command(about = "Rename an agent tracking label.")]
    Rename {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        id: String,
        /// Unique case-sensitive name; trimmed, 4–40 chars, control-free, and not a canonical ID/ref.
        name: String,
    },
    #[command(about = "Read transcript Event JSONL. Default: last 20 message events.")]
    Logs(AgentLogsArgs),
    #[command(about = "Dump the complete current raw model context as JSONL for debugging.")]
    Context(ContextArgs),
    #[command(about = "Compatibility alias for `agents followup`.")]
    Send(SendArgs),
    #[command(about = "Assign durable follow-up work and wake or resume the Agent.")]
    Followup(SendArgs),
    #[command(about = "Interrupt the current turn while preserving resumable Agent identity.")]
    Interrupt {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        id: String,
    },
    #[command(
        about = "Set a working agent deadline in minutes from now.",
        after_help = "JSONL output: one complete Agent object with the updated deadline.\n\nExample:\n  subagent agents time a_7 90"
    )]
    Time {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        id: String,
        /// New deadline from now in integer minutes, from 1 through 6000.
        #[arg(value_name = "MINUTES", value_parser = parse_minutes)]
        minutes: u64,
    },
    #[command(about = "Stop a working agent and all its terminal process groups.")]
    Stop {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        id: String,
    },
    #[command(about = "Permanently delete a non-working agent history.")]
    Delete {
        /// Agent a_N ref, agt_<ULID>, or exact name.
        #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
        id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Readonly,
    Write,
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Working,
    Interrupted,
    Finished,
    Stopped,
    Failed,
}

impl StatusArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Interrupted => "interrupted",
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
    after_help="JSONL output: one complete Agent object containing its preferred a_N ref and stable agt_<ULID>.\n\nExample:\n  subagent agents spawn --name \"API tests\" --dir /home/me/project --mode write --message \"Add API regression tests\""
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
    /// Unique case-sensitive name; trimmed, 4–40 chars, control-free, and not a canonical ID/ref.
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
    after_help = "JSONL output: zero or more compact Agent records followed by one list_summary. Compact records contain {type,id,ref,name,status,dir,mode,model,spawned_at,last_message_at,updated_at,current_phase,last_event_at,run_number,working_sides}; --verbose emits full telemetry.\n\nExamples:\n  subagent agents list --status working --limit 20\n  subagent agents list --limit 100 --after-cursor CURSOR\n  subagent agents list --after-cursor CURSOR --offset 0"
)]
struct ListArgs {
    /// Filter by status; repeat for working, interrupted, finished, stopped, or failed.
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
    /// Maximum Agent records to emit, from 1 through 1000.
    #[arg(long, default_value_t = 100, value_parser = parse_list_limit)]
    limit: usize,
    /// Number of matching Agent records to skip; must be zero with --after-cursor.
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Continue after an opaque cursor from a previous list_summary.
    #[arg(long)]
    after_cursor: Option<String>,
    /// Emit complete lifecycle, deadline, error, and activity diagnostics.
    #[arg(long)]
    verbose: bool,
}

#[derive(Args)]
#[command(
    after_help = "Finite JSONL output ends with logs_summary. Event schema: {owner,event_id,ref,agent_id,agent_ref,sequence,timestamp,type,data}. Types: system_message,user_message,assistant_message,reasoning,tool_call,tool_result,lifecycle,error. Follow mode streams Events only."
)]
struct AgentLogsArgs {
    /// Agent a_N ref, agt_<ULID>, or exact name.
    #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
    id: String,
    /// Exact Event type to include; repeatable. Empty selects system/user/assistant messages.
    #[arg(long = "type", value_name = "EVENT_TYPE", value_parser = ["system_message", "user_message", "assistant_message", "reasoning", "tool_call", "tool_result", "lifecycle", "error"])]
    types: Vec<String>,
    /// Include every Event type. Conflicts with --type.
    #[arg(long, conflicts_with = "types")]
    all: bool,
    /// Emit only events after this e_N ref or evt_<ULID>.
    #[arg(long, value_name = "EVENT_ID", value_parser = parse_event_id)]
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
    after_help = "Finite JSONL output ends with logs_summary. Event schema: {owner,event_id,ref,agent_id,agent_ref,side_id,side_ref,sequence,timestamp,type,data}. Types: system_message,user_message,assistant_message,reasoning,tool_call,tool_result,lifecycle,error. Follow mode streams Events only."
)]
struct SideLogsArgs {
    /// Side short ref (s_N) or durable ID (side_<ULID>).
    #[arg(value_name = "SIDE", value_parser = parse_side_id)]
    id: String,
    /// Exact Event type to include; repeatable. Empty selects user/assistant messages.
    #[arg(long = "type", value_name = "EVENT_TYPE", value_parser = ["system_message", "user_message", "assistant_message", "reasoning", "tool_call", "tool_result", "lifecycle", "error"])]
    types: Vec<String>,
    /// Include every Event type. Conflicts with --type.
    #[arg(long, conflicts_with = "types")]
    all: bool,
    /// Emit only events after this e_N ref or evt_<ULID>.
    #[arg(long, value_name = "EVENT_ID", value_parser = parse_event_id)]
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
    /// Agent a_N ref, agt_<ULID>, or exact name.
    #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
    id: String,
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="JSONL output: one immediate acceptance receipt: message_sent for `agents send`, or followup_sent for `agents followup`. Model delivery continues in the daemon.\n\nExamples:\n  subagent agents followup a_7 --message \"Run the full test suite too\"\n  subagent agents send a_7 --message \"Run the full test suite too\""
)]
struct SendArgs {
    /// Agent a_N ref, agt_<ULID>, or exact name.
    #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
    id: String,
    /// Inline user message. Conflicts with --message-file.
    #[arg(long)]
    message: Option<String>,
    /// Read UTF-8 input from PATH; use - for stdin.
    #[arg(long, value_name = "PATH")]
    message_file: Option<String>,
    /// Optional deadline in integer minutes, from 1 through 6000; resets an active deadline.
    #[arg(long, value_name = "MINUTES", value_parser = parse_minutes)]
    wall_time_minutes: Option<u64>,
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="JSONL output: one message_sent receipt. The message is durable but an inactive Agent is not resumed."
)]
struct BasicMessageArgs {
    /// Agent a_N ref, agt_<ULID>, or exact name.
    #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
    id: String,
    #[arg(long)]
    message: Option<String>,
    /// Read UTF-8 input from PATH; use - for stdin.
    #[arg(long, value_name = "PATH")]
    message_file: Option<String>,
}

#[derive(Args)]
#[command(
    group(clap::ArgGroup::new("input").required(true).multiple(false).args(["message", "message_file"])),
    after_help="JSONL output: one side_created receipt immediately. Inspect progress with `subagent sides status SIDE` or `subagent sides logs SIDE`. The Side trace never enters the parent transcript.\n\nExample:\n  subagent sides create a_7 --message \"Which database does this project use?\""
)]
struct CreateSideArgs {
    /// Parent Agent a_N ref, agt_<ULID>, or exact name.
    #[arg(value_name = "AGENT", value_parser = parse_agent_id)]
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
struct InboxListArgs {
    /// Maximum notifications to emit, from 1 through 100.
    #[arg(long, default_value_t = 20, value_parser = parse_inbox_limit)]
    limit: usize,
    /// Number of matching notifications to skip.
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Continue toward older notifications from a previous inbox_summary.
    #[arg(long)]
    after_cursor: Option<String>,
    /// Minimum priority to include, from 1 through 4.
    #[arg(long, default_value_t = 2, value_parser = parse_priority)]
    priority: u8,
    /// Include notifications for only this Agent ref, durable ID, or exact name.
    #[arg(long, value_name = "AGENT")]
    agent: Option<String>,
    /// Include acknowledged notifications; without --all only unread records are shown.
    #[arg(long)]
    all: bool,
}

#[derive(Subcommand)]
enum InboxCommand {
    #[command(
        about = "List durable notifications newest-first.",
        after_help = "JSONL output: zero or more Notifications followed by one inbox_summary with nullable next_cursor. --priority N includes N and higher. --after-cursor may be combined only with omitted --offset or --offset 0.\n\nExample:\n  subagent inbox list --priority 3 --limit 20"
    )]
    List(InboxListArgs),
    /// Acknowledge one notification and everything older.
    Ack {
        /// Notification sequence number or durable ntf_<ULID>.
        #[arg(value_name = "SEQUENCE_OR_NOTIFICATION_ID")]
        identifier: String,
    },
    /// Stream unread notifications as JSONL.
    Follow {
        /// Resume strictly after this notification sequence.
        #[arg(long)]
        after: Option<u64>,
        /// Minimum priority to include, from 1 through 4.
        #[arg(long, default_value_t = 2, value_parser = parse_priority)]
        priority: u8,
        /// Include notifications for only this Agent ref, durable ID, or exact name.
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
    },
    #[command(about = "Wait for the first matching future notification and exit.")]
    Wait {
        /// Resume strictly after this notification sequence; omitted starts at invocation.
        #[arg(long)]
        after: Option<u64>,
        /// Maximum wait in seconds, from 1 through 86400.
        #[arg(long, default_value_t = 60, value_parser = parse_wait_timeout)]
        timeout_seconds: u64,
        #[arg(long, default_value_t = 2, value_parser = parse_priority)]
        priority: u8,
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
        /// Typed envelope/event type to include; repeatable and ORed.
        #[arg(long = "type", value_name = "TYPE")]
        event_types: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TeamCommand {
    #[command(
        about = "List Agents and Sides, followed by one capacity summary.",
        after_help = "Use --active for a coordinator-safe view containing working, interrupted, and capacity-waiting Agents plus active Sides and their parents. Without --active, complete persisted history is emitted and may be large."
    )]
    List {
        /// Emit only active coordination records instead of complete persisted history.
        #[arg(long)]
        active: bool,
    },
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
            write_stdout(error.to_string().as_bytes())?;
            return Ok(());
        }
        Err(error) => bail!(error.to_string()),
    };
    match cli.command {
        TopCommand::Serve { web_ui_port } => {
            daemon::serve(RuntimeConfig::load()?, web_ui_port).await
        }
        TopCommand::Daemon(DaemonCommand::Start { web_ui_port }) => start_daemon(web_ui_port).await,
        TopCommand::Daemon(DaemonCommand::Status) => request_unchecked(Request::DaemonStatus).await,
        TopCommand::Daemon(DaemonCommand::Stop) => request_unchecked(Request::DaemonStop).await,
        TopCommand::Config(command) => config_command(command).await,
        TopCommand::Inbox(command) => {
            let request_value = match command {
                InboxCommand::Ack { identifier } => Request::InboxAck { identifier },
                InboxCommand::Follow {
                    after,
                    priority,
                    agent,
                } => Request::InboxFollow {
                    after_sequence: after,
                    minimum_priority: priority,
                    agent_id: agent,
                },
                InboxCommand::List(args) => Request::Inbox {
                    limit: args.limit,
                    offset: args.offset,
                    after_cursor: args.after_cursor,
                    minimum_priority: args.priority,
                    agent_id: args.agent,
                    include_acknowledged: args.all,
                },
                InboxCommand::Wait {
                    after,
                    timeout_seconds,
                    priority,
                    agent,
                    event_types,
                } => Request::InboxWait {
                    after_sequence: after,
                    timeout_seconds,
                    minimum_priority: priority,
                    agent_id: agent,
                    event_types,
                },
            };
            request(request_value).await
        }
        TopCommand::Team(TeamCommand::List { active }) => {
            request(Request::TeamList {
                active_only: active,
            })
            .await
        }
        TopCommand::Messages(command) => {
            let req = match command {
                MessagesCommand::Send(args) => Request::MessageSend {
                    id: args.id,
                    message: read_message(args.message, args.message_file).await?,
                },
                MessagesCommand::List {
                    agent_id,
                    statuses,
                    limit,
                    after_cursor,
                } => Request::MessageList {
                    agent_id,
                    statuses,
                    limit,
                    after_cursor,
                },
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
                SidesCommand::Create(args) => Request::SideCreate {
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
                    after_cursor,
                } => Request::SideList {
                    agent_id,
                    statuses: statuses
                        .into_iter()
                        .map(|status| status.as_str().into())
                        .collect(),
                    limit,
                    offset,
                    after_cursor,
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
                AgentsCommand::List(a) => {
                    validate_cursor_offset(a.after_cursor.as_deref(), a.offset)?;
                    Request::AgentList {
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
                            after_cursor: a.after_cursor,
                            verbose: a.verbose,
                        },
                    }
                }
                AgentsCommand::Status { id } => Request::AgentStatus { id },
                AgentsCommand::Wait {
                    id,
                    timeout_seconds,
                } => Request::AgentWait {
                    id,
                    timeout_seconds,
                },
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
                AgentsCommand::Followup(a) => Request::AgentFollowup {
                    id: a.id,
                    message: read_message(a.message, a.message_file).await?,
                    wall_time_minutes: a.wall_time_minutes,
                },
                AgentsCommand::Interrupt { id } => Request::AgentInterrupt { id },
                AgentsCommand::Time { id, minutes } => Request::AgentTime { id, minutes },
                AgentsCommand::Stop { id } => Request::AgentStop { id },
                AgentsCommand::Delete { id } => Request::AgentDelete { id },
            };
            request(req).await
        }
    }
}

async fn config_command(command: ConfigCommand) -> Result<()> {
    let paths = Paths::discover()?;
    let requested_key = match &command {
        ConfigCommand::Get { key } | ConfigCommand::Set { key, .. } => Some(key.clone()),
        ConfigCommand::List => None,
    };
    if let Some(key) = &requested_key
        && !CONFIG_KEYS.contains(&key.as_str())
    {
        return Err(coded_error(
            "invalid_argument",
            format!("unknown config key: {key}"),
            json!({"key":key,"valid_keys":CONFIG_KEYS}),
            false,
        ));
    }
    if let ConfigCommand::Set { key, value } = command {
        let mut cfg = FileConfig::load_persisted(&paths)?;
        cfg.set(&key, &value).map_err(|error| {
            coded_error(
                "invalid_argument",
                format!("{error:#}"),
                json!({"key":key}),
                false,
            )
        })?;
        cfg.save(&paths)?;
    }

    let mut values = local_config_values(&paths)?;
    if let Ok(active) = exchange(Request::ConfigActive).await {
        for value in &mut values {
            let key = value.get("key").and_then(Value::as_str);
            if let Some(active) = active
                .iter()
                .find(|item| item.get("key").and_then(Value::as_str) == key)
            {
                value["active_value"] = active.get("active_value").cloned().unwrap_or(Value::Null);
                value["active_source"] =
                    active.get("active_source").cloned().unwrap_or(Value::Null);
                let (differs, restart_required) = config_change_state(
                    &value["local_effective_value"],
                    &value["active_value"],
                    value["local_source"].as_str(),
                    value["active_source"].as_str(),
                );
                value["active_differs_from_local"] = Value::Bool(differs);
                value["restart_required"] = Value::Bool(restart_required);
            }
        }
    }
    for value in values {
        if requested_key
            .as_deref()
            .is_none_or(|key| value.get("key").and_then(Value::as_str) == Some(key))
        {
            let mut line = serde_json::to_vec(&value)?;
            line.push(b'\n');
            if !write_stdout(&line)? {
                break;
            }
        }
    }
    Ok(())
}

fn config_change_state(
    local_value: &Value,
    active_value: &Value,
    local_source: Option<&str>,
    active_source: Option<&str>,
) -> (bool, bool) {
    let differs = local_value != active_value;
    let configuration_layer =
        |source: Option<&str>| matches!(source, Some("default" | "persisted"));
    (
        differs,
        differs && configuration_layer(local_source) && configuration_layer(active_source),
    )
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
    rotate_daemon_log(&cfg.paths)?;
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
    let mut child = cmd.spawn().context("start daemon")?;
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            return request_unchecked(Request::DaemonStatus).await;
        }
        if let Some(status) = child.try_wait()? {
            return Err(coded_error(
                "daemon_start_failed",
                "daemon exited before becoming ready",
                json!({
                    "pid":child.id(),
                    "exit_status":status.code(),
                    "log_path":cfg.paths.daemon_log(),
                    "failure_summary":daemon_failure_summary(&cfg.paths),
                }),
                true,
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(coded_error(
        "daemon_start_failed",
        "daemon did not become ready within five seconds",
        json!({"pid":child.id(),"log_path":cfg.paths.daemon_log(),"failure_summary":daemon_failure_summary(&cfg.paths)}),
        true,
    ))
}

fn rotate_daemon_log(paths: &Paths) -> Result<()> {
    let path = paths.daemon_log();
    let Ok(metadata) = fs::metadata(&path) else {
        return Ok(());
    };
    if metadata.len() < MAX_DAEMON_LOG_BYTES {
        return Ok(());
    }
    let backup = path.with_extension("log.1");
    if backup.exists() {
        fs::remove_file(&backup)
            .with_context(|| format!("remove old daemon log backup {}", backup.display()))?;
    }
    fs::rename(&path, &backup).with_context(|| format!("rotate daemon log {}", path.display()))?;
    Ok(())
}

async fn request(req: Request) -> Result<()> {
    ensure_protocol_compatible().await?;
    request_unchecked(req).await
}

async fn ensure_protocol_compatible() -> Result<()> {
    let values = exchange(Request::DaemonStatus).await?;
    let daemon = values.first().context("daemon status returned no object")?;
    let actual = daemon.get("protocol_version").and_then(Value::as_u64);
    if actual != Some(u64::from(PROTOCOL_VERSION)) {
        return Err(coded_error(
            "protocol_mismatch",
            "the CLI and running daemon are incompatible; restart the daemon with 'subagent daemon stop' followed by 'subagent daemon start'",
            json!({"cli_version":env!("CARGO_PKG_VERSION"),"cli_protocol_version":PROTOCOL_VERSION,"daemon_version":daemon.get("version"),"daemon_protocol_version":actual}),
            false,
        ));
    }
    Ok(())
}

async fn request_unchecked(req: Request) -> Result<()> {
    let mut lines = send_request(req).await?;
    let mut failed = false;
    while let Some(line) = lines.next_line().await? {
        let value: Value = serde_json::from_str(&line)?;
        let line = serde_json::to_string(&value)?;
        if value.get("type").and_then(Value::as_str) == Some("error") {
            eprintln!("{line}");
            failed = true;
        } else if !write_stdout(format!("{line}\n").as_bytes())? {
            return Ok(());
        }
    }
    if failed {
        std::process::exit(4)
    }
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> Result<bool> {
    let mut stdout = io::stdout().lock();
    match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn exchange(req: Request) -> Result<Vec<Value>> {
    let mut lines = send_request(req).await?;
    let mut values = Vec::new();
    while let Some(line) = lines.next_line().await? {
        values.push(serde_json::from_str(&line)?);
    }
    Ok(values)
}

async fn send_request(req: Request) -> Result<tokio::io::Lines<BufReader<UnixStream>>> {
    let paths = Paths::discover()?;
    let mut stream = UnixStream::connect(paths.socket())
        .await
        .map_err(|_| daemon_connection_error(&paths))?;
    let mut body = serde_json::to_vec(&req)?;
    body.push(b'\n');
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    Ok(BufReader::new(stream).lines())
}

fn daemon_connection_error(paths: &Paths) -> anyhow::Error {
    if let Some(state) = read_daemon_lifecycle(paths).ok().flatten() {
        if matches!(state.status.as_str(), "running" | "starting") && !process_is_alive(state.pid) {
            return coded_error(
                "daemon_crashed",
                "the previously running daemon exited unexpectedly",
                json!({
                    "last_pid":state.pid,
                    "last_status":state.status,
                    "started_at":state.started_at,
                    "last_state_at":state.updated_at,
                    "version":state.version,
                    "log_path":paths.daemon_log(),
                    "failure_summary":daemon_failure_summary(paths),
                }),
                true,
            );
        }
        if state.status == "stopped" {
            return coded_error(
                "daemon_stopped",
                "daemon is stopped; run 'subagent daemon start'",
                json!({
                    "last_pid":state.pid,
                    "stopped_at":state.updated_at,
                    "version":state.version,
                    "log_path":paths.daemon_log(),
                }),
                true,
            );
        }
        if state.status == "shutdown_failed" {
            return coded_error(
                "shutdown_failed",
                "the daemon encountered an error while stopping workers",
                json!({
                    "last_pid":state.pid,
                    "failed_at":state.updated_at,
                    "version":state.version,
                    "log_path":paths.daemon_log(),
                    "failure_summary":daemon_failure_summary(paths),
                }),
                true,
            );
        }
    }
    coded_error(
        "daemon_unavailable",
        "daemon is not running; run 'subagent daemon start'",
        json!({"socket":paths.socket(),"log_path":paths.daemon_log()}),
        true,
    )
}

fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn daemon_failure_summary(paths: &Paths) -> Option<String> {
    let body = fs::read_to_string(paths.daemon_log()).ok()?;
    let mut summary = body
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim()
        .to_string();
    if summary.chars().count() > 500 {
        summary = summary.chars().take(500).collect();
    }
    Some(summary)
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

fn parse_list_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "limit must be an integer from 1 through 1000".to_string())?;
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return Err("limit must be an integer from 1 through 1000".into());
    }
    Ok(limit)
}

fn validate_cursor_offset(after_cursor: Option<&str>, offset: usize) -> Result<()> {
    if after_cursor.is_some() && offset != 0 {
        return Err(coded_error(
            "invalid_argument",
            "--after-cursor may only be combined with --offset 0; remove --offset or set it to 0",
            json!({"fields":["after_cursor","offset"],"offset":offset}),
            false,
        ));
    }
    Ok(())
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
        .map_err(|_| "priority must be an integer from 1 through 4".to_string())?;
    if !(1..=4).contains(&priority) {
        return Err("priority must be an integer from 1 through 4".into());
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

fn parse_wait_timeout(value: &str) -> std::result::Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "timeout must be an integer from 1 through 86400".to_string())?;
    if !(1..=86_400).contains(&seconds) {
        return Err("timeout must be an integer from 1 through 86400".into());
    }
    Ok(seconds)
}

fn parse_typed_id(value: &str, prefix: &str, label: &str) -> std::result::Result<String, String> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        let received = value.split_once('_').map_or(value, |(head, _)| head);
        return Err(format!(
            "expected {label} beginning with {prefix}, but received {received}_..."
        ));
    };
    if suffix.parse::<ulid::Ulid>().is_err() {
        return Err(format!(
            "expected {label} as {prefix}<26-character ULID>, but received {value}"
        ));
    }
    Ok(value.to_string())
}

fn parse_agent_id(value: &str) -> std::result::Result<String, String> {
    if !value.trim().is_empty() {
        Ok(value.to_string())
    } else {
        Err("AGENT must not be empty".into())
    }
}
fn parse_side_id(value: &str) -> std::result::Result<String, String> {
    if valid_local_ref(value, "s_") {
        Ok(value.to_string())
    } else {
        parse_typed_id(value, "side_", "SIDE")
    }
}
fn parse_message_id(value: &str) -> std::result::Result<String, String> {
    if valid_local_ref(value, "m_") {
        Ok(value.to_string())
    } else {
        parse_typed_id(value, "msg_", "MESSAGE_ID")
    }
}
fn parse_event_id(value: &str) -> std::result::Result<String, String> {
    if valid_local_ref(value, "e_") {
        Ok(value.to_string())
    } else {
        parse_typed_id(value, "evt_", "EVENT_ID")
    }
}

fn valid_local_ref(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.bytes().all(|byte| byte.is_ascii_digit())
            && suffix.parse::<u64>().is_ok_and(|number| number > 0)
    })
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

    #[test]
    fn list_limit_is_positive_and_bounded() {
        assert_eq!(parse_list_limit("1").unwrap(), 1);
        assert_eq!(parse_list_limit("1000").unwrap(), 1_000);
        for value in ["0", "1001", "18446744073709551615"] {
            assert!(parse_list_limit(value).is_err(), "{value}");
        }
    }

    #[test]
    fn cursor_accepts_only_zero_offset() {
        assert!(validate_cursor_offset(None, 12).is_ok());
        assert!(validate_cursor_offset(Some("cursor"), 0).is_ok());
        let error = validate_cursor_offset(Some("cursor"), 1).unwrap_err();
        let value = error_json_for(&error, "cli_error");
        assert_eq!(value["code"], "invalid_argument");
        assert!(value["message"].as_str().unwrap().contains("--offset 0"));
    }

    #[test]
    fn btw_alias_is_not_accepted() {
        let error = match Cli::try_parse_from(["subagent", "agents", "btw", "--help"]) {
            Ok(_) => panic!("btw must remain removed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unrecognized subcommand 'btw'"));
    }

    #[test]
    fn restart_required_ignores_environment_divergence() {
        assert_eq!(
            config_change_state(
                &json!("local"),
                &json!("active"),
                Some("default"),
                Some("OPENAI_MODEL"),
            ),
            (true, false)
        );
        assert_eq!(
            config_change_state(
                &json!(65_000),
                &json!(64_000),
                Some("persisted"),
                Some("default"),
            ),
            (true, true)
        );
        assert_eq!(
            config_change_state(
                &json!("same"),
                &json!("same"),
                Some("persisted"),
                Some("persisted"),
            ),
            (false, false)
        );
    }

    #[test]
    fn daemon_log_rotates_at_ten_mebibytes() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
            runtime_dir: temp.path().join("run"),
        };
        fs::create_dir_all(&paths.state_dir).unwrap();
        let log = paths.daemon_log();
        let file = fs::File::create(&log).unwrap();
        file.set_len(MAX_DAEMON_LOG_BYTES).unwrap();

        rotate_daemon_log(&paths).unwrap();

        assert!(!log.exists());
        assert_eq!(
            fs::metadata(log.with_extension("log.1")).unwrap().len(),
            MAX_DAEMON_LOG_BYTES
        );
    }
}
