use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementCapabilities {
    pub protocol_version: u32,
    pub route: &'static str,
    pub request_ids: bool,
    pub structured_errors: bool,
    pub event_cursor: bool,
    pub methods: Vec<&'static str>,
    pub method_specs: Vec<MethodSpec>,
    pub events: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodRoute {
    Acp,
    Dwo,
    Both,
}

impl MethodRoute {
    pub fn allows(self, route: &str) -> bool {
        matches!(
            (self, route),
            (Self::Acp, "acp") | (Self::Dwo, "dwo") | (Self::Both, "acp") | (Self::Both, "dwo")
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodOperation {
    Query,
    Command,
    Subscription,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodSpec {
    pub name: &'static str,
    pub route: MethodRoute,
    pub operation: MethodOperation,
    pub side_effect: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<&'static str>,
}

const fn query(name: &'static str, route: MethodRoute) -> MethodSpec {
    MethodSpec {
        name,
        route,
        operation: MethodOperation::Query,
        side_effect: false,
        event: None,
    }
}

const fn command(
    name: &'static str,
    route: MethodRoute,
    event: Option<&'static str>,
) -> MethodSpec {
    MethodSpec {
        name,
        route,
        operation: MethodOperation::Command,
        side_effect: true,
        event,
    }
}

const fn subscription(name: &'static str, route: MethodRoute) -> MethodSpec {
    MethodSpec {
        name,
        route,
        operation: MethodOperation::Subscription,
        side_effect: false,
        event: None,
    }
}

pub const METHOD_SPECS: &[MethodSpec] = &[
    query("dwo.capabilities", MethodRoute::Dwo),
    query("event.read", MethodRoute::Dwo),
    subscription("event.subscribe", MethodRoute::Dwo),
    query("daemon.status", MethodRoute::Dwo),
    command("daemon.shutdown", MethodRoute::Dwo, None),
    query("config.snapshot", MethodRoute::Dwo),
    command("config.update", MethodRoute::Dwo, Some("config.changed")),
    query("session.list", MethodRoute::Both),
    query("session.status-list", MethodRoute::Dwo),
    query("session.status", MethodRoute::Dwo),
    query("session.snapshot", MethodRoute::Both),
    query("session.prompt-directives", MethodRoute::Both),
    command("session.new", MethodRoute::Both, None),
    command("session.fork", MethodRoute::Both, None),
    query("session.read", MethodRoute::Dwo),
    command("session.delete", MethodRoute::Both, None),
    command("session.keep", MethodRoute::Dwo, None),
    command("session.close", MethodRoute::Both, None),
    command("session.set_config_option", MethodRoute::Acp, None),
    command("session.notify", MethodRoute::Acp, None),
    command("session.compact", MethodRoute::Acp, None),
    command("session.resume-turn", MethodRoute::Acp, None),
    command("session.prompt", MethodRoute::Acp, None),
    command("session.cancel", MethodRoute::Acp, None),
    query("session.options", MethodRoute::Acp),
    command("session.permission", MethodRoute::Acp, None),
    subscription("session.watch", MethodRoute::Acp),
    query("project.list", MethodRoute::Dwo),
    query("project.get", MethodRoute::Dwo),
    query("project.board", MethodRoute::Dwo),
    command("project.create", MethodRoute::Dwo, Some("project.changed")),
    command("project.update", MethodRoute::Dwo, Some("project.changed")),
    command(
        "project.section.create",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.section.update",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.section.delete",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.section.reorder",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    query("project.topic.get", MethodRoute::Dwo),
    command(
        "project.topic.create",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.update",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.delete",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.move",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.reorder",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    query("project.topic.overview.get", MethodRoute::Dwo),
    command(
        "project.topic.overview.set",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    query("project.topic.agents.get", MethodRoute::Dwo),
    command(
        "project.topic.agents.set",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.session.assign",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.session.unassign",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.task.assign",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.task.unassign",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.topic.task.create",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.label.create",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.label.update",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.label.delete",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.label.assign",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    command(
        "project.label.unassign",
        MethodRoute::Dwo,
        Some("project.changed"),
    ),
    query("automation.list", MethodRoute::Dwo),
    query("automation.status", MethodRoute::Dwo),
    command(
        "automation.update",
        MethodRoute::Dwo,
        Some("automation.changed"),
    ),
    query("automation.history", MethodRoute::Dwo),
    command(
        "automation.add",
        MethodRoute::Dwo,
        Some("automation.changed"),
    ),
    command(
        "automation.enable",
        MethodRoute::Dwo,
        Some("automation.changed"),
    ),
    command(
        "automation.disable",
        MethodRoute::Dwo,
        Some("automation.changed"),
    ),
    command(
        "automation.delete",
        MethodRoute::Dwo,
        Some("automation.changed"),
    ),
    command("automation.run", MethodRoute::Dwo, Some("automation.run")),
    query("mcp.list", MethodRoute::Dwo),
    query("mcp.config", MethodRoute::Dwo),
    query("mcp.search", MethodRoute::Dwo),
    command("mcp.call", MethodRoute::Dwo, None),
    command("mcp.auth.login", MethodRoute::Dwo, Some("mcp.status")),
    command("mcp.auth.logout", MethodRoute::Dwo, Some("mcp.status")),
    command("mcp.auth.unauth", MethodRoute::Dwo, Some("mcp.status")),
    command("mcp.enable", MethodRoute::Dwo, Some("mcp.status")),
    command("mcp.disable", MethodRoute::Dwo, Some("mcp.status")),
    command("mcp.install", MethodRoute::Dwo, Some("mcp.status")),
    command("mcp.uninstall", MethodRoute::Dwo, Some("mcp.status")),
    query("skill.list", MethodRoute::Dwo),
    command("skill.enable", MethodRoute::Dwo, Some("skill.changed")),
    command("skill.disable", MethodRoute::Dwo, Some("skill.changed")),
    command("skill.install", MethodRoute::Dwo, Some("skill.changed")),
    command("skill.uninstall", MethodRoute::Dwo, Some("skill.changed")),
    query("model.list", MethodRoute::Dwo),
    command(
        "model.set_default",
        MethodRoute::Dwo,
        Some("config.changed"),
    ),
    command("model.upsert", MethodRoute::Dwo, Some("config.changed")),
    command("model.remove", MethodRoute::Dwo, Some("config.changed")),
    query("provider.list", MethodRoute::Dwo),
    query("model.catalog.list", MethodRoute::Dwo),
    command(
        "model.catalog.upsert",
        MethodRoute::Dwo,
        Some("config.changed"),
    ),
    command(
        "model.catalog.remove",
        MethodRoute::Dwo,
        Some("config.changed"),
    ),
    command("provider.upsert", MethodRoute::Dwo, Some("config.changed")),
    command("provider.remove", MethodRoute::Dwo, Some("config.changed")),
    query("prompt.list", MethodRoute::Dwo),
    query("prompt.get", MethodRoute::Dwo),
    command("prompt.set", MethodRoute::Dwo, Some("config.changed")),
    query("rule.list", MethodRoute::Dwo),
    query("rule.get", MethodRoute::Dwo),
    command("rule.set", MethodRoute::Dwo, Some("config.changed")),
    query("channel.list", MethodRoute::Dwo),
    query("channel.<kind>.status", MethodRoute::Dwo),
    command(
        "channel.<kind>.enable",
        MethodRoute::Dwo,
        Some("channel.status"),
    ),
    command(
        "channel.<kind>.disable",
        MethodRoute::Dwo,
        Some("channel.status"),
    ),
    command(
        "channel.<kind>.config",
        MethodRoute::Dwo,
        Some("channel.status"),
    ),
    command(
        "channel.<kind>.begin",
        MethodRoute::Dwo,
        Some("channel.status"),
    ),
    command(
        "channel.<kind>.bind",
        MethodRoute::Dwo,
        Some("channel.status"),
    ),
    command("channel.<kind>.poll", MethodRoute::Dwo, None),
    command(
        "channel.<kind>.unbind",
        MethodRoute::Dwo,
        Some("channel.status"),
    ),
    command(
        "channel.<kind>.remove",
        MethodRoute::Dwo,
        Some("channel.status"),
    ),
    command("channel.<kind>.send_message", MethodRoute::Dwo, None),
    command("channel.<kind>.send_file", MethodRoute::Dwo, None),
    query("websocket.status", MethodRoute::Dwo),
    command(
        "websocket.enable",
        MethodRoute::Dwo,
        Some("websocket.status"),
    ),
    command(
        "websocket.disable",
        MethodRoute::Dwo,
        Some("websocket.status"),
    ),
    command(
        "websocket.config",
        MethodRoute::Dwo,
        Some("websocket.status"),
    ),
    query("websocket.token", MethodRoute::Dwo),
    command(
        "websocket.reset_token",
        MethodRoute::Dwo,
        Some("websocket.status"),
    ),
];

pub const EVENTS: &[&str] = &[
    "config.changed",
    "config.apply_failed",
    "mcp.status",
    "automation.changed",
    "automation.run",
    "channel.status",
    "websocket.status",
    "skill.changed",
    "project.changed",
];

const CHANNEL_KINDS: &[&str] = &["weixin", "telegram", "feishu", "qq"];

fn method_matches(pattern: &str, method: &str) -> bool {
    let Some((prefix, suffix)) = pattern.split_once("<kind>") else {
        return pattern == method;
    };
    let Some(kind) = method
        .strip_prefix(prefix)
        .and_then(|remaining| remaining.strip_suffix(suffix))
    else {
        return false;
    };
    CHANNEL_KINDS.contains(&kind)
}

pub fn method_spec(method: &str) -> Option<&'static MethodSpec> {
    METHOD_SPECS
        .iter()
        .find(|spec| method_matches(spec.name, method))
}

pub fn method_allowed(route: &str, method: &str) -> bool {
    method_spec(method).is_some_and(|spec| spec.route.allows(route))
}

pub fn is_side_effect_method(method: &str) -> bool {
    method_spec(method).is_some_and(|spec| spec.side_effect)
}

pub fn capabilities() -> ManagementCapabilities {
    let specs = METHOD_SPECS
        .iter()
        .filter(|spec| spec.route.allows("dwo"))
        .copied()
        .collect::<Vec<_>>();
    ManagementCapabilities {
        protocol_version: 3,
        route: "dwo",
        request_ids: true,
        structured_errors: true,
        event_cursor: true,
        methods: specs.iter().map(|spec| spec.name).collect(),
        method_specs: specs,
        events: EVENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_registry_keeps_chat_on_acp() {
        assert!(method_allowed("acp", "session.prompt"));
        assert!(!method_allowed("dwo", "session.prompt"));
        assert!(method_allowed("dwo", "channel.telegram.status"));
        assert!(method_allowed("dwo", "websocket.status"));
        assert!(!method_allowed("dwo", "channel.unknown.status"));
    }

    #[test]
    fn only_mutations_are_idempotency_candidates() {
        assert!(is_side_effect_method("session.prompt"));
        assert!(is_side_effect_method("mcp.call"));
        assert!(!is_side_effect_method("dwo.capabilities"));
    }

    #[test]
    fn method_specs_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in METHOD_SPECS {
            assert!(seen.insert(spec.name), "duplicate method: {}", spec.name);
        }
    }
}
