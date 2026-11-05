//! GraphQL: a request whose method is `GRAPHQL`, as JetBrains' HTTP client
//! writes it, sent as the JSON `POST` a GraphQL server expects.
//!
//! ```text
//! GRAPHQL https://countries.trevorblades.com/
//!
//! query Country($code: ID!) { country(code: $code) { name capital } }
//!
//! {"code": "UG"}
//! ```
//!
//! The body is the query, then, after a blank line, its variables as a JSON
//! object if it has any. Written that way it is kept that way, in the file,
//! the journal and history; only what goes out is JSON. Every GraphQL request
//! is also checked for `errors` in its answer, which servers send with a 200.

use serde_json::{Map, Value};
use tracer_protocol::{CheckResult, GqlField, GqlSchema, GqlType, HttpRequest, HttpResponse};

use crate::expect;

pub const METHOD: &str = "GRAPHQL";

/// The check every GraphQL request gets.
pub const NO_ERRORS: &str = "no GraphQL errors";

pub fn is_graphql(request: &HttpRequest) -> bool {
    request.method.eq_ignore_ascii_case(METHOD)
}

/// The query and, if the body ends with one, its variables object.
pub fn split(body: &str) -> (String, Option<Value>) {
    let body = body.trim();
    if let Some(at) = body.rfind("\n\n").or_else(|| body.rfind("\r\n\r\n")) {
        let (query, rest) = (body[..at].trim(), body[at..].trim());
        if let Ok(vars @ Value::Object(_)) = serde_json::from_str::<Value>(rest) {
            if !query.is_empty() {
                return (query.to_string(), Some(vars));
            }
        }
    }
    (body.to_string(), None)
}

/// Whether `query` changes something: a mutation, or a subscription, which
/// this client sends as one request and doesn't keep open.
pub fn is_write(query: &str) -> bool {
    let first = query
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("");
    first.starts_with("mutation") || first.starts_with("subscription")
}

/// The WebSocket subprotocol subscriptions go over (the `graphql-ws`
/// library's, which Apollo, Hasura, Yoga and others speak).
pub const SUBPROTOCOL: &str = "graphql-transport-ws";

/// Whether `request` is a GraphQL subscription, which goes over a
/// WebSocket and stays open.
pub fn is_subscription(request: &HttpRequest) -> bool {
    is_graphql(request) && first_word(&split(request.body.as_deref().unwrap_or("")).0) == "subscription"
}

fn first_word(query: &str) -> &str {
    let line = query.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("");
    line.split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or("")
}

/// A subscription as the WebSocket it's sent over: its URL's `http` made
/// `ws` (`https`, `wss`), the subprotocol asked for, and its opening: the
/// `connection_init`, then, once the server has answered (its
/// `connection_ack`), the `subscribe`. Each `next` the server sends is a
/// message of the stream; its `complete` closes it.
pub fn to_subscription(request: &HttpRequest) -> Result<HttpRequest, String> {
    let (query, variables) = split(request.body.as_deref().unwrap_or(""));
    let url = request.url.trim();
    let lower = url.to_ascii_lowercase();
    let url = if lower.starts_with("https://") {
        format!("wss://{}", &url[8..])
    } else if lower.starts_with("http://") {
        format!("ws://{}", &url[7..])
    } else {
        url.to_string()
    };
    let mut payload = Map::new();
    payload.insert("query".into(), Value::String(query));
    if let Some(vars) = variables {
        payload.insert("variables".into(), vars);
    }
    let init = serde_json::json!({ "type": "connection_init", "payload": {} });
    let subscribe = serde_json::json!({ "id": "1", "type": "subscribe", "payload": Value::Object(payload) });
    let mut headers = request.headers.clone();
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-protocol")) {
        headers.push(("Sec-WebSocket-Protocol".into(), SUBPROTOCOL.into()));
    }
    Ok(HttpRequest {
        method: tracer_protocol::WEBSOCKET.into(),
        url,
        headers,
        body: Some(format!("===\n{init}\n=== wait-for-server\n{subscribe}\n")),
        ..request.clone()
    })
}

/// The "no GraphQL errors" check of a subscription, over every message it
/// got: an `error` message, or a `next` whose payload has errors, fails it.
pub fn subscription_check(received: &[String]) -> CheckResult {
    let mut errors: Vec<String> = Vec::new();
    let mut data = 0;
    for text in received {
        let Ok(message) = serde_json::from_str::<Value>(text) else { continue };
        match message.get("type").and_then(Value::as_str) {
            Some("error") => errors.push(message.get("payload").map(Value::to_string).unwrap_or_default()),
            Some("next") => {
                let payload = message.get("payload");
                match payload.and_then(|p| p.get("errors")).and_then(Value::as_array) {
                    Some(list) if !list.is_empty() => {
                        errors.extend(list.iter().filter_map(|e| e.get("message")?.as_str().map(String::from)));
                    }
                    _ => data += 1,
                }
            }
            _ => {}
        }
    }
    let (pass, actual) = if errors.is_empty() {
        (true, format!("none in {data} result{}", if data == 1 { "" } else { "s" }))
    } else {
        let n = errors.len();
        (false, format!("{n} error{}: {}", if n == 1 { "" } else { "s" }, errors.into_iter().take(3).collect::<Vec<_>>().join("; ")))
    };
    CheckResult { expect: NO_ERRORS.into(), pass, actual }
}

/// `request`, its variables filled in, as it goes out: a JSON `POST`.
pub fn to_http(request: &HttpRequest) -> Result<HttpRequest, String> {
    let (query, variables) = split(request.body.as_deref().unwrap_or(""));
    if query.is_empty() {
        return Err("a GraphQL request needs a query in its body".into());
    }
    let mut payload = Map::new();
    payload.insert("query".into(), Value::String(query));
    if let Some(vars) = variables {
        payload.insert("variables".into(), vars);
    }
    let mut headers = request.headers.clone();
    for (name, value) in [("Content-Type", "application/json"), ("Accept", "application/json")] {
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
            headers.push((name.into(), value.into()));
        }
    }
    Ok(HttpRequest { method: "POST".into(), headers, body: Some(Value::Object(payload).to_string()), ..request.clone() })
}

/// `request`'s expectations checked against `response`, and for a GraphQL
/// request, whether the answer has errors.
pub fn checks(request: &HttpRequest, response: Result<&HttpResponse, &str>) -> Vec<CheckResult> {
    let mut out = expect::check(&request.expect, response);
    // A subscription's is judged over its messages (`subscription_check`).
    if is_graphql(request) && !is_subscription(request) {
        out.push(errors_check(response));
    }
    out
}

fn errors_check(response: Result<&HttpResponse, &str>) -> CheckResult {
    let (pass, actual) = match response {
        Err(why) => (false, format!("no answer: {why}")),
        Ok(r) => match serde_json::from_str::<Value>(&r.body) {
            Err(_) => (false, format!("status {}, and the body is not JSON", r.status)),
            Ok(json) => match json.get("errors").and_then(Value::as_array) {
                Some(errors) if !errors.is_empty() => {
                    let said: Vec<&str> = errors.iter().filter_map(|e| e.get("message")?.as_str()).take(3).collect();
                    let n = errors.len();
                    (false, format!("{n} error{}: {}", if n == 1 { "" } else { "s" }, said.join("; ")))
                }
                _ if json.get("data").is_some() => (true, "none".into()),
                _ => (false, "neither data nor errors in the answer".into()),
            },
        },
    };
    CheckResult { expect: NO_ERRORS.into(), pass, actual }
}

/// The schema in an answer to [`tracer_protocol::GRAPHQL_INTROSPECTION`], the server's own
/// `__` types left out.
pub fn schema(body: &str) -> Result<GqlSchema, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "the answer is not JSON".to_string())?;
    if let Some(errors) = json.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
        let said: Vec<&str> = errors.iter().filter_map(|e| e.get("message")?.as_str()).take(3).collect();
        return Err(format!("the server refused to describe itself: {}", said.join("; ")));
    }
    let s = json.pointer("/data/__schema").ok_or("the answer has no data.__schema")?;
    let root = |key: &str| s.get(key).and_then(|t| t.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
    let mut types = Vec::new();
    for t in s.get("types").and_then(Value::as_array).into_iter().flatten() {
        let name = t.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() || name.starts_with("__") {
            continue;
        }
        let fields = t
            .get("fields")
            .and_then(Value::as_array)
            .or_else(|| t.get("inputFields").and_then(Value::as_array))
            .into_iter()
            .flatten()
            .map(|f| GqlField {
                name: text(f, "name"),
                args: f
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|a| format!("{}: {}", text(a, "name"), type_name(a.get("type"))))
                    .collect::<Vec<_>>()
                    .join(", "),
                ty: type_name(f.get("type")),
                description: text(f, "description"),
            })
            .collect();
        let names = |key: &str| -> Vec<String> {
            t.get(key).and_then(Value::as_array).into_iter().flatten().map(|v| text(v, "name")).collect()
        };
        let mut values = names("enumValues");
        values.extend(names("possibleTypes"));
        types.push(GqlType {
            name: name.to_string(),
            kind: text(t, "kind"),
            description: text(t, "description"),
            fields,
            values,
        });
    }
    Ok(GqlSchema { query: root("queryType"), mutation: root("mutationType"), subscription: root("subscriptionType"), types })
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `[User!]!` from introspection's nested `ofType`s.
fn type_name(t: Option<&Value>) -> String {
    let Some(t) = t else { return "?".into() };
    match t.get("kind").and_then(Value::as_str) {
        Some("NON_NULL") => format!("{}!", type_name(t.get("ofType"))),
        Some("LIST") => format!("[{}]", type_name(t.get("ofType"))),
        _ => t.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_subscription_goes_over_a_websocket() {
        let sub = HttpRequest {
            method: "GRAPHQL".into(),
            url: "https://api.test/graphql".into(),
            body: Some("# ticks\nsubscription Ticks($n: Int) { tick(every: $n) }\n\n{\"n\": 2}".into()),
            ..Default::default()
        };
        assert!(is_subscription(&sub));
        assert!(!is_subscription(&HttpRequest { body: Some("query { a }".into()), ..sub.clone() }));
        assert!(!is_subscription(&HttpRequest { body: Some("subscriptions { a }".into()), ..sub.clone() }));
        let ws = to_subscription(&sub).unwrap();
        assert_eq!((ws.method.as_str(), ws.url.as_str()), ("WEBSOCKET", "wss://api.test/graphql"));
        assert!(ws.headers.contains(&("Sec-WebSocket-Protocol".into(), SUBPROTOCOL.into())));
        let opening = crate::realtime::opening(ws.body.as_deref().unwrap());
        assert_eq!(opening[0], (false, r#"{"payload":{},"type":"connection_init"}"#.to_string()));
        assert!(opening[1].0, "the subscribe waits for the server's ack");
        let subscribe: Value = serde_json::from_str(&opening[1].1).unwrap();
        assert_eq!(subscribe["type"], "subscribe");
        assert_eq!(subscribe["payload"]["variables"], json!({ "n": 2 }));
        assert!(subscribe["payload"]["query"].as_str().unwrap().contains("tick(every: $n)"));
        // Its errors check is the stream's, not the 101's empty body.
        assert!(checks(&sub, Ok(&HttpResponse { status: 101, headers: vec![], body: String::new(), size: 0, timing: tracer_protocol::Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None }, tls_unchecked: false })).is_empty());
    }

    #[test]
    fn a_subscriptions_errors_are_in_its_messages() {
        let ok = [r#"{"type":"connection_ack"}"#, r#"{"id":"1","type":"next","payload":{"data":{"tick":1}}}"#, r#"{"id":"1","type":"next","payload":{"data":{"tick":2}}}"#];
        let check = subscription_check(&ok.map(String::from));
        assert_eq!((check.pass, check.actual.as_str()), (true, "none in 2 results"));
        let bad = [r#"{"id":"1","type":"next","payload":{"errors":[{"message":"boom"}]}}"#, r#"{"id":"1","type":"error","payload":[{"message":"no"}]}"#];
        let check = subscription_check(&bad.map(String::from));
        assert!(!check.pass && check.actual.starts_with("2 errors: boom; "), "{}", check.actual);
    }
    use tracer_protocol::Timing;

    fn gql(body: &str) -> HttpRequest {
        HttpRequest { method: "GRAPHQL".into(), url: "http://x/graphql".into(), body: Some(body.into()), ..Default::default() }
    }

    #[test]
    fn a_body_is_its_query_then_its_variables() {
        let (q, v) = split("query A($id: ID!) {\n  user(id: $id) { name }\n}\n\n{\"id\": \"7\"}\n");
        assert_eq!(q, "query A($id: ID!) {\n  user(id: $id) { name }\n}");
        assert_eq!(v, Some(json!({"id": "7"})));
        // A query that starts with `{` is no variables object.
        assert_eq!(split("{ users { id } }"), ("{ users { id } }".into(), None));
        assert_eq!(split("{ a }\n\n{ b }").1, None, "`{{ b }}` is not JSON");

        let sent = to_http(&gql("{ a }\n\n{\"x\": 1}")).unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(serde_json::from_str::<Value>(sent.body.as_deref().unwrap()).unwrap(), json!({"query": "{ a }", "variables": {"x": 1}}));
        assert!(sent.headers.iter().any(|(k, v)| k == "Content-Type" && v == "application/json"));
        assert!(to_http(&gql("  ")).is_err());
    }

    #[test]
    fn a_mutation_writes_and_a_query_reads() {
        assert!(is_write("# make one\nmutation { add(x: 1) { id } }"));
        assert!(!is_write("query { a }"));
        assert!(!is_write("{ a }"));
    }

    #[test]
    fn errors_in_a_200_fail_the_check() {
        let answer = |body: &str| HttpResponse {
            status: 200,
            headers: vec![],
            body: body.into(),
            size: 0,
            timing: Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None },
            tls_unchecked: false,
        };
        let r = gql("{ a }");
        let ok = checks(&r, Ok(&answer(r#"{"data":{"a":1}}"#)));
        assert_eq!((ok.len(), ok[0].pass), (1, true));
        let bad = checks(&r, Ok(&answer(r#"{"data":null,"errors":[{"message":"no field a"}]}"#)));
        assert_eq!((bad[0].pass, bad[0].actual.as_str()), (false, "1 error: no field a"));
        assert!(!checks(&r, Ok(&answer("<html>")))[0].pass);
        let rest = HttpRequest { method: "GET".into(), ..r };
        assert!(checks(&rest, Ok(&answer("<html>"))).is_empty(), "only GraphQL requests get it");
    }

    #[test]
    fn a_schema_is_read_from_introspection() {
        let body = json!({"data": {"__schema": {
            "queryType": {"name": "Query"}, "mutationType": null, "subscriptionType": null,
            "types": [
                {"kind": "OBJECT", "name": "Query", "description": null, "fields": [
                    {"name": "country", "description": "One", "args": [{"name": "code", "type": {"kind": "NON_NULL", "name": null, "ofType": {"kind": "SCALAR", "name": "ID"}}}],
                     "type": {"kind": "OBJECT", "name": "Country"}},
                    {"name": "countries", "description": null, "args": [],
                     "type": {"kind": "NON_NULL", "name": null, "ofType": {"kind": "LIST", "name": null, "ofType": {"kind": "NON_NULL", "name": null, "ofType": {"kind": "OBJECT", "name": "Country"}}}}}
                ], "inputFields": null, "enumValues": null, "possibleTypes": null},
                {"kind": "ENUM", "name": "Size", "description": null, "fields": null, "inputFields": null,
                 "enumValues": [{"name": "S"}, {"name": "L"}], "possibleTypes": null},
                {"kind": "OBJECT", "name": "__Type", "description": null, "fields": [], "inputFields": null, "enumValues": null, "possibleTypes": null}
            ]}}});
        let s = schema(&body.to_string()).unwrap();
        assert_eq!((s.query.as_str(), s.mutation.as_str(), s.types.len()), ("Query", "", 2));
        let q = &s.types[0];
        assert_eq!((q.fields[0].args.as_str(), q.fields[0].ty.as_str()), ("code: ID!", "Country"));
        assert_eq!(q.fields[1].ty, "[Country!]!");
        assert_eq!(s.types[1].values, vec!["S", "L"]);
        assert!(s.to_sdl().contains("type Query {\n  \"One\"\n  country(code: ID!): Country\n  countries: [Country!]!\n}"));
        assert!(schema(r#"{"errors":[{"message":"introspection is off"}]}"#).unwrap_err().contains("introspection is off"));
    }
}
