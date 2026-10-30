//! What agents may do on their own, and what waits for a person.
//!
//! Agents drive Tracer as freely as the window does, within limits. Some
//! commands are a person's alone (secrets, deleting an environment,
//! approving, the kill switch, stopping the core); while the kill switch is
//! off, agents may only read. The rest is the workspace's policy
//! ([`crate::policy`]): where an agent's requests may go, which wait for a
//! person to allow them (by default, any that changes something, and any
//! sent with a production environment), how many it may start, and how big
//! a body it may send. Every agent action is in the journal under its name,
//! which is the audit log.

use std::time::Duration;

use tracer_protocol::{Access, Actor, AgentRules, ApproveWhen, ClientKind, Command, HttpRequest};

/// How long a request waits for a person before it fails.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Methods that only read, which an agent may send without asking.
const SAFE_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// Why an agent with `rules` sending `request` with `env` needs a person
/// to allow it, if it does.
pub fn needs_approval(rules: &AgentRules, request: &HttpRequest, env: Option<&str>) -> Option<String> {
    let method = request.method.to_ascii_uppercase();
    if rules.approve.contains(&ApproveWhen::All) {
        return Some(format!("{method} from an agent whose every request is approved"));
    }
    let unsafe_method = rules.approve.contains(&ApproveWhen::Writes) && !SAFE_METHODS.contains(&method.as_str());
    let prod = env.filter(|e| rules.approve.contains(&ApproveWhen::Prod) && is_production(e));
    match (unsafe_method, prod) {
        (true, Some(env)) => Some(format!("{method} from an agent, with environment {env}")),
        (true, None) => Some(format!("{method} from an agent")),
        (false, Some(env)) => Some(format!("an agent using environment {env}")),
        (false, None) => None,
    }
}

/// Why an agent with `rules` may not send `request`, as it will go out:
/// variables filled in.
pub fn outside_rules(rules: &AgentRules, request: &HttpRequest) -> Option<String> {
    if let Some(max) = rules.max_body {
        let size = request.body.as_ref().map_or(0, |b| b.len() as u64);
        if size > max {
            return Some(format!("its body is {size} bytes; the policy allows {max}"));
        }
    }
    if rules.access == Access::Local {
        match host(&request.url) {
            Some(host) if is_local(&host) => {}
            Some(host) => return Some(format!("{host} is not this machine, and the policy keeps this agent local")),
            None => return Some(format!("`{}` names no host", request.url)),
        }
    }
    None
}

fn host(url: &str) -> Option<String> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    Some(uri.host()?.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase())
}

/// `localhost`, `*.localhost`, `127.x.x.x` and `::1`.
fn is_local(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `prod`, `production`, `prod-eu` and the like.
pub fn is_production(env: &str) -> bool {
    env.to_ascii_lowercase().starts_with("prod")
}

/// Why `actor`, held to `access`, may not send `command`, if it may not.
pub fn refused(actor: &Actor, command: &Command, agents_allowed: bool, access: Access) -> Option<String> {
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
            | Command::Policy
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
    if access == Access::ReadOnly {
        return Some(format!("the workspace's policy lets {} only read", actor.name));
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

    fn rules(policy: &str) -> AgentRules {
        crate::policy::Policy::parse(policy).unwrap().for_agent("claude-code")
    }

    #[test]
    fn an_agent_asks_before_changing_anything_or_touching_production() {
        let rules = rules("");
        assert_eq!(needs_approval(&rules, &request("get"), Some("local")), None);
        assert_eq!(needs_approval(&rules, &request("POST"), None).as_deref(), Some("POST from an agent"));
        assert_eq!(
            needs_approval(&rules, &request("GET"), Some("Prod-EU")).as_deref(),
            Some("an agent using environment Prod-EU")
        );
    }

    #[test]
    fn the_policy_says_what_waits_for_a_person() {
        let none = rules("[*]\napprove = none\n");
        assert_eq!(needs_approval(&none, &request("DELETE"), Some("prod")), None);
        let prod = rules("[*]\napprove = prod\n");
        assert_eq!(needs_approval(&prod, &request("DELETE"), None), None);
        assert!(needs_approval(&prod, &request("GET"), Some("production")).is_some());
        let all = rules("[*]\napprove = all\n");
        assert!(needs_approval(&all, &request("GET"), None).is_some());
    }

    #[test]
    fn a_local_agent_reaches_only_this_machine() {
        let local = rules("[claude-code]\naccess = local\n");
        let to = |url: &str| HttpRequest { url: url.into(), ..request("GET") };
        for url in ["http://localhost:8080/x", "http://api.localhost/", "http://127.0.0.2/", "http://[::1]:3000/"] {
            assert_eq!(outside_rules(&local, &to(url)), None, "{url}");
        }
        for url in ["https://example.com/", "http://10.0.0.5/", "http://localhost.evil.com/", "not a url"] {
            assert!(outside_rules(&local, &to(url)).is_some(), "{url}");
        }
        assert_eq!(outside_rules(&rules(""), &to("https://example.com/")), None);
    }

    #[test]
    fn a_body_over_the_cap_is_refused() {
        let capped = rules("[*]\nmax_body = 4\n");
        let with = |body: &str| HttpRequest { body: Some(body.into()), ..request("POST") };
        assert_eq!(outside_rules(&capped, &with("1234")), None);
        assert!(outside_rules(&capped, &with("12345")).unwrap().contains("5 bytes"));
    }

    #[test]
    fn switched_off_an_agent_may_only_read() {
        let send = Command::Send { request: request("GET"), env: None };
        assert_eq!(refused(&agent(), &send, true, Access::Anywhere), None);
        assert!(refused(&agent(), &send, false, Access::Anywhere).is_some());
        assert_eq!(refused(&agent(), &Command::ListRequests, false, Access::Anywhere), None);
        let secret = Command::SetSecret { env: "a".into(), name: "b".into(), value: None };
        assert!(refused(&agent(), &secret, true, Access::Anywhere).unwrap().contains("for a person"));
    }

    #[test]
    fn a_read_only_agent_may_only_read() {
        let send = Command::Send { request: request("GET"), env: None };
        assert!(refused(&agent(), &send, true, Access::ReadOnly).unwrap().contains("only read"));
        assert_eq!(refused(&agent(), &Command::Policy, true, Access::ReadOnly), None);
        let person = Actor { kind: ClientKind::Window, name: "you".into() };
        assert_eq!(refused(&person, &send, true, Access::ReadOnly), None);
    }
}
