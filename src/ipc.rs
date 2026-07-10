use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    DaemonStatus,
    DaemonStop,
    AgentSpawn {
        dir: String,
        message: String,
        title: Option<String>,
        mode: AgentMode,
        wall_time_hours: Option<f64>,
    },
    AgentList {
        filter: ListFilter,
    },
    AgentStatus {
        id: String,
    },
    AgentLogs {
        id: String,
        types: Vec<String>,
        after: Option<String>,
        limit: usize,
        follow: bool,
    },
    AgentContext {
        id: String,
        include: Vec<String>,
        max_tokens: usize,
    },
    AgentSend {
        id: String,
        message: String,
        wall_time_hours: Option<f64>,
    },
    AgentSide {
        id: String,
        message: String,
        wall_time_hours: Option<f64>,
    },
    AgentTime {
        id: String,
        hours: f64,
    },
    AgentStop {
        id: String,
    },
    AgentDelete {
        id: String,
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
pub struct ErrorLine<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub code: &'a str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

pub fn error_json(code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::to_value(ErrorLine {
        kind: "error",
        code,
        message: message.into(),
        details: None,
    })
    .unwrap()
}
