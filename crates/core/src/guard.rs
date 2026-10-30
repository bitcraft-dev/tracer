//! What agents may do on their own, and what waits for a person.
//!
//! Agents drive Tracer as freely as the window does, with three limits: a
//! request that changes something (anything but `GET`, `HEAD` or
//! `OPTIONS`), or one sent with a production environment, waits for a person
//! to allow it; some commands are a person's alone (secrets, deleting an
//! environment, approving, the kill switch, stopping the core); and while
//! the kill switch is off, agents may only read. Every agent action is in
//! the journal under its name, which is the audit log.

use std::time::Duration;

use tracer_protocol::{Actor, ClientKind, Command, HttpRequest};

/// How long a request waits for a person before it fails.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Methods that only read, which an agent may send without asking.
const SAFE_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// Why `actor` sending `request` with `env` needs a person to allow it, if
/// it does.
pub fn needs_approval(actor: &Actor, request: &HttpRequest, env: Option<&str>) -> Option<String> {
    if actor.kind != ClientKind::Agent {
        return None;
    }
    let method = request.method.to_ascii_uppercase();
    let unsafe_method = !SAFE_METHODS.contains(&method.as_str());
    let prod = env.filter(|e| is_production(e));
    match (unsafe_method, prod) {
        (true, Some(env)) => Some(format!("{method} from an agent, with environment {env}")),
        (true, None) => Some(format!("{method} from an agent")),
        (false, Some(env)) => Some(format!("an agent using environment {env}")),
        (false, None) => None,
    }
}

/// `prod`, `production`, `prod-eu` and the like.
pub fn is_production(env: &str) -> bool {
    env.to_ascii_lowercase().starts_with("prod")
}

/// Why `actor` may not send `command`, if it may not.
pub fn refused(actor: &Actor, command: &Command, agents_allowed: bool) -> Option<String> {
    if actor.kind != ClientKind::Agent {
        return None;
    }
    let reads = matches!(
        command,
        Command::Subscribe { .. }
            | Command::ListRequests
            | Command::ListEnvironments
            | Command::History { .. }
            | Command::Response { .. }
            | Command::Status
    );
    if reads {
        return None;
    }
    let persons_only = match command {
        Command::SetSecret { .. } => Some("setting a secret"),
        Command::DeleteEnvironment { .. } => Some("deleting an environment"),
        Command::Decide { .. } => Some("approving a request"),
        Command::SetAgents { .. } => Some("switching agents on or off"),
        Command::Shutdown => Some("stopping the core"),
        _ => None,
    };
    if let Some(what) = persons_only {
        return Some(format!("{what} is for a person, not an agent"));
    }
    if !agents_allowed {
        return Some("agents are switched off in this workspace; a person can switch them back on".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> Actor {
        Actor { kind: ClientKind::Agent, name: "claude-code".into() }
    }

    fn request(method: &str) -> HttpRequest {
        HttpRequest { method: method.into(), url: "http://x/".into(), headers: vec![], body: None }
    }

    #[test]
    fn an_agent_asks_before_changing_anything_or_touching_production() {
        assert_eq!(needs_approval(&agent(), &request("get"), Some("local")), None);
        assert_eq!(needs_approval(&agent(), &request("POST"), None).as_deref(), Some("POST from an agent"));
        assert_eq!(
            needs_approval(&agent(), &request("GET"), Some("Prod-EU")).as_deref(),
            Some("an agent using environment Prod-EU")
        );
        let person = Actor { kind: ClientKind::Window, name: "you".into() };
        assert_eq!(needs_approval(&person, &request("DELETE"), Some("prod")), None);
    }

    #[test]
    fn switched_off_an_agent_may_only_read() {
        let send = Command::Send { request: request("GET"), env: None };
        assert_eq!(refused(&agent(), &send, true), None);
        assert!(refused(&agent(), &send, false).is_some());
        assert_eq!(refused(&agent(), &Command::ListRequests, false), None);
        let secret = Command::SetSecret { env: "a".into(), name: "b".into(), value: None };
        assert!(refused(&agent(), &secret, true).unwrap().contains("for a person"));
    }
}
