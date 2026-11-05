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

use tracer_protocol::{Access, Actor, AgentRules, ApproveWhen, BenchPlan, ClientKind, Command, HttpRequest};

/// How long a request waits for a person before it fails.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Methods that only read, which an agent may send without asking.
const SAFE_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// Why an agent with `rules` sending `request` with `env`, as the first of
/// `bench` if it is, needs a person to allow it, if it does.
pub fn needs_approval(rules: &AgentRules, request: &HttpRequest, env: Option<&str>, bench: Option<&BenchPlan>) -> Option<String> {
    let method = request.method.to_ascii_uppercase();
    // A GraphQL query reads; a mutation writes.
    let writes = if crate::graphql::is_graphql(request) {
        crate::graphql::is_write(request.body.as_deref().unwrap_or(""))
    } else {
        !SAFE_METHODS.contains(&method.as_str())
    };
    let method = if crate::graphql::is_graphql(request) && writes { "GraphQL mutation".to_string() } else { method };
    let reason = if rules.approve.contains(&ApproveWhen::All) {
        Some(format!("{method} from an agent whose every request is approved"))
    } else {
        let unsafe_method = rules.approve.contains(&ApproveWhen::Writes) && writes;
        let prod = env.filter(|e| rules.approve.contains(&ApproveWhen::Prod) && is_production(e));
        match (unsafe_method, prod) {
            (true, Some(env)) => Some(format!("{method} from an agent, with environment {env}")),
            (true, None) => Some(format!("{method} from an agent")),
            (false, Some(env)) => Some(format!("an agent using environment {env}")),
            (false, None) => None,
        }
    };
    // A bench says what it is, whatever the reason it's asked about.
    match (reason, bench) {
        (Some(why), Some(plan)) => Some(format!("{why}: a bench of {}", plan.describe())),
        (None, Some(plan)) if rules.approve.contains(&ApproveWhen::Bench) => {
            Some(format!("a bench from an agent: {}", plan.describe()))
        }
        (reason, _) => reason,
    }
}

/// The most a bench may send, and for how long, whoever starts it.
pub const BENCH_MAX_TOTAL: u64 = 1_000_000;
pub const BENCH_MAX_CONCURRENCY: u32 = 256;
pub const BENCH_MAX_SECS: u64 = 60 * 60;

/// Why `plan` can't run, for anyone, or for an agent held to `rules`.
pub fn bench_refused(plan: &BenchPlan, rules: Option<&AgentRules>) -> Option<String> {
    if plan.total.is_none() && plan.duration_secs.is_none() {
        return Some("a bench needs a total, a duration, or both".into());
    }
    if plan.total == Some(0) || plan.duration_secs == Some(0) || plan.concurrency == 0 {
        return Some("a bench's total, duration and concurrency are at least 1".into());
    }
    if plan.rate.is_some_and(|r| !(r > 0.0 && r.is_finite())) {
        return Some("a bench's rate is a number of requests a second above 0".into());
    }
    if plan.total.is_some_and(|t| t > BENCH_MAX_TOTAL) {
        return Some(format!("a bench sends at most {BENCH_MAX_TOTAL} requests"));
    }
    if plan.concurrency > BENCH_MAX_CONCURRENCY {
        return Some(format!("a bench has at most {BENCH_MAX_CONCURRENCY} requests in flight"));
    }
    if plan.duration_secs.is_some_and(|s| s > BENCH_MAX_SECS) {
        return Some(format!("a bench runs for at most {BENCH_MAX_SECS} s"));
    }
    let rules = rules?;
    if let Some(max) = rules.max_concurrency {
        if plan.concurrency > max {
            return Some(format!("{} at a time is more than the policy's max_concurrency of {max}", plan.concurrency));
        }
    }
    if let Some(max) = rules.max_bench {
        match plan.total {
            Some(total) if total > max => {
                return Some(format!("{total} requests is more than the policy's max_bench of {max}"))
            }
            // Only a duration: it could send any number.
            None => return Some(format!("the policy's max_bench of {max} needs a bench with a total")),
            _ => {}
        }
    }
    None
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
            | Command::Diff { .. }
            | Command::GraphqlSchema { .. }
            | Command::StreamMessages { .. }
            | Command::ListBaselines
            | Command::CompareBench { .. }
            | Command::Status
            | Command::Cookies
            | Command::Export { .. }
            | Command::Import { .. }
            | Command::Explain { .. }
            | Command::LocalServers { .. }
    );
    if reads {
        return None;
    }
    let persons_only = match command {
        Command::SetSecret { .. } => Some("setting a secret"),
        Command::DeleteEnvironment { .. } => Some("deleting an environment"),
        Command::Decide { .. } => Some("approving a request"),
        Command::SetAgents { .. } => Some("switching agents on or off"),
        Command::SetBrowser { .. } => Some("changing how requests behave like a browser's"),
        Command::ClearCookies { .. } => Some("clearing cookies"),
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
        HttpRequest { method: method.into(), url: "http://x/".into(), headers: vec![], body: None, ..Default::default() }
    }

    fn rules(policy: &str) -> AgentRules {
        crate::policy::Policy::parse(policy).unwrap().for_agent("claude-code")
    }

    #[test]
    fn an_agent_asks_before_changing_anything_or_touching_production() {
        let rules = rules("");
        assert_eq!(needs_approval(&rules, &request("get"), Some("local"), None), None);
        assert_eq!(needs_approval(&rules, &request("POST"), None, None).as_deref(), Some("POST from an agent"));
        assert_eq!(
            needs_approval(&rules, &request("GET"), Some("Prod-EU"), None).as_deref(),
            Some("an agent using environment Prod-EU")
        );
    }

    #[test]
    fn the_policy_says_what_waits_for_a_person() {
        let none = rules("[*]\napprove = none\n");
        assert_eq!(needs_approval(&none, &request("DELETE"), Some("prod"), None), None);
        let prod = rules("[*]\napprove = prod\n");
        assert_eq!(needs_approval(&prod, &request("DELETE"), None, None), None);
        assert!(needs_approval(&prod, &request("GET"), Some("production"), None).is_some());
        let all = rules("[*]\napprove = all\n");
        assert!(needs_approval(&all, &request("GET"), None, None).is_some());
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
    fn a_graphql_query_reads_and_a_mutation_asks() {
        let gql = |body: &str| HttpRequest { body: Some(body.into()), ..request("GRAPHQL") };
        assert_eq!(needs_approval(&rules(""), &gql("{ users { id } }"), None, None), None);
        let asked = needs_approval(&rules(""), &gql("mutation { del(id: 1) }"), None, None);
        assert_eq!(asked.as_deref(), Some("GraphQL mutation from an agent"));
    }

    #[test]
    fn a_bench_is_asked_about_and_held_to_its_caps() {
        let plan = BenchPlan { total: Some(500), concurrency: 10, ..BenchPlan::default() };
        let asked = needs_approval(&rules(""), &request("GET"), None, Some(&plan));
        assert_eq!(asked.as_deref(), Some("a bench from an agent: 500 requests, 10 at a time"));
        let writes = needs_approval(&rules(""), &request("POST"), None, Some(&plan)).unwrap();
        assert_eq!(writes, "POST from an agent: a bench of 500 requests, 10 at a time");
        assert_eq!(needs_approval(&rules("[*]\napprove = none\n"), &request("GET"), None, Some(&plan)), None);

        let capped = rules("[*]\nmax_bench = 100\nmax_concurrency = 10\n");
        assert!(bench_refused(&plan, Some(&capped)).unwrap().contains("max_bench of 100"));
        let small = BenchPlan { total: Some(100), ..plan.clone() };
        assert_eq!(bench_refused(&small, Some(&capped)), None);
        let wide = BenchPlan { concurrency: 11, ..small.clone() };
        assert!(bench_refused(&wide, Some(&capped)).unwrap().contains("max_concurrency"));
        let timed = BenchPlan { total: None, duration_secs: Some(10), ..small.clone() };
        assert!(bench_refused(&timed, Some(&capped)).unwrap().contains("with a total"));
        assert_eq!(bench_refused(&timed, None), None);
        for bad in [
            BenchPlan { concurrency: 1, ..BenchPlan::default() },
            BenchPlan { total: Some(1), concurrency: 0, ..BenchPlan::default() },
            BenchPlan { total: Some(1), concurrency: 1, rate: Some(0.0), ..BenchPlan::default() },
        ] {
            assert!(bench_refused(&bad, None).is_some(), "{bad:?}");
        }
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
