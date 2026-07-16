use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    DaemonStatus,
    DaemonStop,
    AgentSpawn {
        dir: String,
        message: String,
        name: String,
        mode: AgentMode,
        model: Option<String>,
        wall_time_minutes: Option<u64>,
    },
    AgentList {
        filter: ListFilter,
    },
    AgentStatus {
        id: String,
    },
    AgentRename {
        id: String,
        name: String,
    },
    AgentLogs {
        id: String,
        types: Vec<String>,
        all: bool,
        after: Option<String>,
        limit: usize,
        follow: bool,
    },
    AgentContext {
        id: String,
    },
    AgentSend {
        id: String,
        message: String,
        wall_time_minutes: Option<u64>,
    },
    AgentSide {
        id: String,
        message: String,
        model: Option<String>,
        wall_time_minutes: Option<u64>,
    },
    Inbox {
        limit: usize,
        offset: usize,
        minimum_priority: u8,
        agent_id: Option<String>,
    },
    SideList {
        agent_id: String,
        statuses: Vec<String>,
        limit: usize,
        offset: usize,
    },
    SideStatus {
        id: String,
    },
    SideLogs {
        id: String,
        types: Vec<String>,
        all: bool,
        after: Option<String>,
        limit: usize,
        follow: bool,
    },
    SideStop {
        id: String,
    },
    SideDelete {
        id: String,
    },
    AgentTime {
        id: String,
        minutes: u64,
    },
    AgentStop {
        id: String,
    },
    AgentDelete {
        id: String,
    },
    MessageList {
        agent_id: String,
        statuses: Vec<String>,
    },
    MessageStatus {
        agent_id: String,
        message_id: String,
    },
    MessageCancel {
        agent_id: String,
        message_id: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMode {
    Readonly,
    Write,
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Readonly => "readonly",
            Self::Write => "write",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListFilter {
    pub statuses: Vec<String>,
    pub dir: Option<String>,
    pub spawned_after: Option<String>,
    pub spawned_before: Option<String>,
    pub finished_after: Option<String>,
    pub finished_before: Option<String>,
    pub sort: String,
    pub order: String,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Serialize)]
pub struct ErrorLine {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub code: String,
    pub message: String,
    pub details: Value,
    pub retryable: bool,
}

#[derive(Debug)]
pub struct CodedError {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
    pub retryable: bool,
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

pub fn coded_error(
    code: &'static str,
    message: impl Into<String>,
    details: Value,
    retryable: bool,
) -> anyhow::Error {
    CodedError {
        code,
        message: message.into(),
        details,
        retryable,
    }
    .into()
}

pub fn error_json_for(error: &anyhow::Error, fallback_code: &str) -> Value {
    let coded = error.downcast_ref::<CodedError>();
    serde_json::to_value(ErrorLine {
        kind: "error",
        code: coded.map_or_else(|| fallback_code.into(), |error| error.code.into()),
        message: format!("{error:#}"),
        details: coded.map_or_else(|| json!({}), |error| error.details.clone()),
        retryable: coded.is_some_and(|error| error.retryable),
    })
    .unwrap()
}
