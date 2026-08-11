use anyhow::Result;
use dwo_context::MessageContent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionCommand {
    Compact,
    Resume,
    Fork,
    Status,
}

pub(crate) fn parse_session_command(content: &MessageContent) -> Result<Option<SessionCommand>> {
    let Some(text) = content.as_text() else {
        return Ok(None);
    };
    let mut parts = text.split_whitespace();
    let Some(name) = parts.next() else {
        return Ok(None);
    };
    match name {
        "/compact" => {
            anyhow::ensure!(parts.next().is_none(), "/compact does not accept input");
            Ok(Some(SessionCommand::Compact))
        }
        "/resume" => {
            anyhow::ensure!(parts.next().is_none(), "/resume does not accept input");
            Ok(Some(SessionCommand::Resume))
        }
        "/fork" => {
            anyhow::ensure!(parts.next().is_none(), "/fork does not accept input");
            Ok(Some(SessionCommand::Fork))
        }
        "/status" => {
            anyhow::ensure!(parts.next().is_none(), "/status does not accept input");
            Ok(Some(SessionCommand::Status))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_session_commands_without_claiming_other_slash_prompts() {
        assert_eq!(
            parse_session_command(&MessageContent::text(" /compact ")).unwrap(),
            Some(SessionCommand::Compact)
        );
        assert_eq!(
            parse_session_command(&MessageContent::text("/resume")).unwrap(),
            Some(SessionCommand::Resume)
        );
        assert_eq!(
            parse_session_command(&MessageContent::text("/fork")).unwrap(),
            Some(SessionCommand::Fork)
        );
        assert_eq!(
            parse_session_command(&MessageContent::text(" /status ")).unwrap(),
            Some(SessionCommand::Status)
        );
        assert_eq!(
            parse_session_command(&MessageContent::text("/skill review inspect")).unwrap(),
            None
        );
        assert_eq!(
            parse_session_command(&MessageContent::text("/mcp github search")).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_arguments_for_local_session_commands() {
        for (command, error) in [
            ("/compact now", "/compact does not accept input"),
            ("/resume now", "/resume does not accept input"),
            ("/fork now", "/fork does not accept input"),
            ("/status now", "/status does not accept input"),
        ] {
            assert_eq!(
                parse_session_command(&MessageContent::text(command))
                    .unwrap_err()
                    .to_string(),
                error
            );
        }
    }
}
