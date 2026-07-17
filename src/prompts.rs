use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

pub const AGENT: &str = include_str!("../prompts/agent.md");
pub const AGENT_READONLY: &str = include_str!("../prompts/agent-readonly.md");
pub const AGENT_WRITE: &str = include_str!("../prompts/agent-write.md");
pub const SIDE: &str = include_str!("../prompts/side.md");
pub const CONTEXT_COMPACTION: &str = include_str!("../prompts/context-compaction.md");
pub const EMPTY_COMPLETION_RETRY: &str = include_str!("../prompts/empty-completion-retry.md");

pub fn render(template: &str, values: &[(&str, &str)]) -> Result<String> {
    let values = values.iter().copied().collect::<BTreeMap<_, _>>();
    let mut used = BTreeSet::new();
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("unterminated prompt placeholder");
        };
        let name = &after[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            bail!("invalid prompt placeholder: {name}");
        }
        let value = values
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("missing prompt value: {name}"))?;
        out.push_str(value);
        used.insert(name);
        rest = &after[end + 2..];
    }
    if rest.contains("}}") {
        bail!("unmatched prompt placeholder terminator");
    }
    out.push_str(rest);
    if let Some(name) = values.keys().find(|name| !used.contains(**name)) {
        bail!("unknown prompt value: {name}");
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_named_values_strictly() {
        assert_eq!(render("Hi {{name}}", &[("name", "Ada")]).unwrap(), "Hi Ada");
        assert!(render("{{missing}}", &[]).is_err());
        assert!(render("plain", &[("extra", "value")]).is_err());
        assert!(render("{{Bad}}", &[("Bad", "value")]).is_err());
        assert!(render("{{open", &[]).is_err());
    }

    #[test]
    fn bundled_prompts_render() {
        render(
            AGENT,
            &[
                ("working_directory", "/tmp/work"),
                ("mode_instructions", "Write mode."),
            ],
        )
        .unwrap();
        render(AGENT_READONLY, &[]).unwrap();
        render(AGENT_WRITE, &[]).unwrap();
        render(SIDE, &[("working_directory", "/tmp/work")]).unwrap();
        render(CONTEXT_COMPACTION, &[("summary", "older work")]).unwrap();
        render(EMPTY_COMPLETION_RETRY, &[]).unwrap();
    }
}
