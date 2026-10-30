//! The workspace's agent policy: `agents.policy`, what AI agents may do.
//!
//! ```text
//! # Every agent, unless a section below names it.
//! [*]
//! access = anywhere        # read-only | local | anywhere
//! approve = writes, prod   # writes, prod, all, or none
//! rate = 60/min            # N/s, N/min, N/h, or none
//! max_body = 1 MiB         # B, KB, KiB, MB, MiB, or none
//!
//! [claude-code]
//! access = local
//! ```
//!
//! A section sets only what it names; the rest comes from `[*]`, and what
//! `[*]` leaves out is as with no policy at all: anywhere, writes and prod
//! approved, no caps. Agent names match ignoring case.
//!
//! The name is the one an agent connects under, which it chooses, and a
//! process that can read the workspace can read the core's token. The policy
//! keeps a well-meaning agent within bounds; it is no wall against one set
//! on getting round it.

use tracer_protocol::{Access, AgentRules, ApproveWhen, Rate};

/// The rules for every agent first, then those for agents by name, each
/// whole: a named section's gaps already filled from `[*]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    rules: Vec<AgentRules>,
}

/// One section as written: only what it names.
#[derive(Default)]
struct Section {
    agent: String,
    access: Option<Access>,
    approve: Option<Vec<ApproveWhen>>,
    rate: Option<Option<Rate>>,
    max_body: Option<Option<u64>>,
}

impl Default for Policy {
    /// No policy file: what Tracer did before there was one.
    fn default() -> Policy {
        Policy { rules: vec![everyone(Access::Anywhere)] }
    }
}

impl Policy {
    /// What a core gets when the policy file is there but can't be read:
    /// agents may read and nothing more, until a person fixes it.
    pub fn read_only() -> Policy {
        Policy { rules: vec![everyone(Access::ReadOnly)] }
    }

    pub fn rules(&self) -> &[AgentRules] {
        &self.rules
    }

    /// The rules for the agent connecting as `name`.
    pub fn for_agent(&self, name: &str) -> AgentRules {
        let found = self.rules[1..].iter().find(|r| r.agent.eq_ignore_ascii_case(name));
        found.unwrap_or(&self.rules[0]).clone()
    }

    pub fn parse(text: &str) -> Result<Policy, String> {
        let mut sections: Vec<Section> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let at = |why: String| format!("line {}: {why}", n + 1);
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[') {
                let name = name.strip_suffix(']').ok_or_else(|| at("a section is `[name]`".into()))?.trim();
                if name.is_empty() {
                    return Err(at("a section names an agent, or `*` for every agent".into()));
                }
                if sections.iter().any(|s| s.agent.eq_ignore_ascii_case(name)) {
                    return Err(at(format!("`[{name}]` is there twice")));
                }
                sections.push(Section { agent: name.to_string(), ..Section::default() });
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| at("expected `key = value`".into()))?;
            let (key, value) = (key.trim(), value.trim());
            let section = sections.last_mut().ok_or_else(|| at("a setting comes after `[*]` or `[agent]`".into()))?;
            match key {
                "access" => section.access = Some(access(value).map_err(at)?),
                "approve" => section.approve = Some(approve(value).map_err(at)?),
                "rate" => section.rate = Some(none_or(value, rate).map_err(at)?),
                "max_body" => section.max_body = Some(none_or(value, size).map_err(at)?),
                _ => return Err(at(format!("`{key}` is not a setting: access, approve, rate or max_body"))),
            }
        }

        let mut all = everyone(Access::Anywhere);
        if let Some(star) = sections.iter().find(|s| s.agent == "*") {
            apply(&mut all, star);
        }
        let mut rules = vec![all.clone()];
        for section in sections.iter().filter(|s| s.agent != "*") {
            let mut named = AgentRules { agent: section.agent.clone(), ..all.clone() };
            apply(&mut named, section);
            rules.push(named);
        }
        Ok(Policy { rules })
    }
}

fn everyone(access: Access) -> AgentRules {
    AgentRules {
        agent: "*".into(),
        access,
        approve: vec![ApproveWhen::Writes, ApproveWhen::Prod],
        rate: None,
        max_body: None,
    }
}

fn apply(rules: &mut AgentRules, section: &Section) {
    if let Some(access) = section.access {
        rules.access = access;
    }
    if let Some(approve) = &section.approve {
        rules.approve = approve.clone();
    }
    if let Some(rate) = section.rate {
        rules.rate = rate;
    }
    if let Some(max_body) = section.max_body {
        rules.max_body = max_body;
    }
}

fn access(value: &str) -> Result<Access, String> {
    match value.to_ascii_lowercase().as_str() {
        "read-only" => Ok(Access::ReadOnly),
        "local" => Ok(Access::Local),
        "anywhere" => Ok(Access::Anywhere),
        _ => Err(format!("access is read-only, local or anywhere, not `{value}`")),
    }
}

fn approve(value: &str) -> Result<Vec<ApproveWhen>, String> {
    if value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for word in value.split(',').map(str::trim) {
        let when = match word.to_ascii_lowercase().as_str() {
            "writes" => ApproveWhen::Writes,
            "prod" => ApproveWhen::Prod,
            "all" => ApproveWhen::All,
            _ => return Err(format!("approve takes writes, prod, all or none, not `{word}`")),
        };
        if !out.contains(&when) {
            out.push(when);
        }
    }
    Ok(out)
}

fn none_or<T>(value: &str, parse: fn(&str) -> Result<T, String>) -> Result<Option<T>, String> {
    if value.eq_ignore_ascii_case("none") {
        Ok(None)
    } else {
        parse(value).map(Some)
    }
}

/// `60/min`, `5/s`, `1000/h`.
fn rate(value: &str) -> Result<Rate, String> {
    let bad = || format!("rate is a count per s, min or h, such as `60/min`, not `{value}`");
    let (count, per) = value.split_once('/').ok_or_else(bad)?;
    let count: u32 = count.trim().parse().map_err(|_| bad())?;
    let per_secs = match per.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" => 1,
        "min" | "m" => 60,
        "h" | "hour" => 3600,
        _ => return Err(bad()),
    };
    if count == 0 {
        return Err("a rate of 0 lets nothing through; write `access = read-only` for that".into());
    }
    Ok(Rate { count, per_secs })
}

/// `512`, `64 KiB`, `1MB`.
fn size(value: &str) -> Result<u64, String> {
    let bad = || format!("max_body is a size such as `1 MiB` or `64 KB`, not `{value}`");
    let digits = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    let n: u64 = value[..digits].parse().map_err(|_| bad())?;
    let unit = match value[digits..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1000,
        "kib" => 1024,
        "mb" => 1000 * 1000,
        "mib" => 1024 * 1024,
        _ => return Err(bad()),
    };
    Ok(n * unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_section_fills_its_gaps_from_every_agent() {
        let policy = Policy::parse(
            "# shared\n[*]\nrate = 60/min   # a minute\nmax_body = 1 MiB\n\n[Claude-Code]\naccess = local\napprove = none\n",
        )
        .unwrap();
        let all = policy.for_agent("cursor");
        assert_eq!(all.access, Access::Anywhere);
        assert_eq!(all.approve, vec![ApproveWhen::Writes, ApproveWhen::Prod]);
        assert_eq!(all.rate, Some(Rate { count: 60, per_secs: 60 }));
        let claude = policy.for_agent("claude-code");
        assert_eq!(claude.agent, "Claude-Code");
        assert_eq!(claude.access, Access::Local);
        assert!(claude.approve.is_empty());
        assert_eq!(claude.rate, all.rate);
        assert_eq!(claude.max_body, Some(1024 * 1024));
    }

    #[test]
    fn a_section_before_every_agent_still_inherits_from_it() {
        let policy = Policy::parse("[bot]\napprove = all\n[*]\naccess = read-only\n").unwrap();
        assert_eq!(policy.for_agent("bot").access, Access::ReadOnly);
        assert_eq!(policy.for_agent("bot").approve, vec![ApproveWhen::All]);
    }

    #[test]
    fn a_mistake_names_its_line() {
        for (text, line) in [
            ("access = local\n", "line 1"),
            ("[*]\n\naccess = nowhere\n", "line 3"),
            ("[*]\nrate = 10/day\n", "line 2"),
            ("[*]\nmax_body = 2 GiB\n", "line 2"),
            ("[*]\ncolour = red\n", "line 2"),
            ("[a]\n[A]\n", "line 2"),
        ] {
            let err = Policy::parse(text).unwrap_err();
            assert!(err.starts_with(line), "{text:?} gave {err:?}");
        }
    }
}
