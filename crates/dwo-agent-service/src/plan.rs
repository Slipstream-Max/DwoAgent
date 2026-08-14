use dwo_context::MessageContent;
use dwo_tools::{PlanEntry, PlanEntryStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub id: String,
    pub entries: Vec<PlanEntry>,
}

impl ExecutionPlan {
    pub fn new(entries: Vec<PlanEntry>) -> Self {
        Self {
            id: format!("plan-{}", Uuid::new_v4()),
            entries,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.entries.iter().all(|entry| entry.status.is_terminal())
    }

    pub fn terminalized(&self) -> Self {
        let mut plan = self.clone();
        for entry in &mut plan.entries {
            if !entry.status.is_terminal() {
                entry.status = PlanEntryStatus::Cancelled;
            }
        }
        plan
    }

    pub fn watcher_message(&self) -> MessageContent {
        MessageContent::text(format!(
            "<execution_plan>\n{}\n</execution_plan>",
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| format!(
                    "  <entry index=\"{}\" priority=\"{}\" status=\"{}\">{}</entry>",
                    index + 1,
                    enum_name(entry.priority),
                    enum_name(entry.status),
                    escape_xml(&entry.content)
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn enum_name<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
