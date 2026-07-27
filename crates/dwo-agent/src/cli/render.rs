use anyhow::Result;
use dwo_agent_service::SessionRecord;
use serde_json::Value;

pub fn print_value(value: &Value) -> Result<()> {
    print!("{}", render_value(value));
    Ok(())
}

pub fn render_value(value: &Value) -> String {
    serde_yaml::to_string(value).unwrap_or_else(|_| "value: <unrenderable>\n".to_string())
}

pub fn print_session_list(value: &Value) -> Result<()> {
    let records: Vec<SessionRecord> = serde_json::from_value(value.clone())?;
    if records.is_empty() {
        println!("No sessions");
        return Ok(());
    }

    for record in records {
        println!("{}", record.info.id);
        println!("  title: {}", yaml_scalar(&record.info.title));
    }
    Ok(())
}

fn yaml_scalar(value: &str) -> String {
    let rendered = serde_yaml::to_string(value).unwrap_or_else(|_| "''\n".to_string());
    rendered
        .strip_prefix("---\n")
        .unwrap_or(&rendered)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_scalar_quotes_ambiguous_values() {
        assert_eq!(yaml_scalar("true"), "'true'");
        assert_eq!(yaml_scalar("plain title"), "plain title");
    }
}
