//! The operator's Flows page: list, read, check and save flows. A flow holds no code and no URLs,
//! so the worst a bad save does is narrow one company's tools.

use nschannel_http::http::{Request, Response};
use nscore::flow::{FlowDef, Guard};

use crate::flowlib;

/// The flow endpoints, or None when the request is not one of them. Called after the token check.
pub(super) fn route(admin: &super::Admin, request: &Request) -> Option<Response> {
    let root = admin.root.as_path();
    let path = request.path();
    let method = request.method.as_str();
    match (method, path) {
        ("GET", "/api/flows") => {
            return Some(match flowlib::list(root) {
                Ok(flows) => Response::json(200, &serde_json::json!({ "flows": flows })),
                Err(e) => server_error(e),
            });
        }
        ("POST", "/api/validate-flow") => {
            return Some(
                match serde_json::from_slice::<serde_json::Value>(&request.body) {
                    Ok(body) => {
                        let report = check_flow(&body);
                        Response::json(
                            200,
                            &serde_json::json!({
                                "ok": report.errors.is_empty(),
                                "errors": report.errors,
                                "warnings": report.warnings,
                            }),
                        )
                    }
                    Err(e) => super::bad_request(format!("the body is not JSON: {e}")),
                },
            );
        }
        _ => {}
    }
    let segments: Vec<&str> = path.strip_prefix("/api/flows/")?.split('/').collect();
    let shape = matches!(
        (method, segments.as_slice()),
        ("GET", [_]) | ("POST", [_]) | ("GET", [_, "versions"]) | ("DELETE", [_, "versions", _])
    );
    if !shape {
        return Some(Response::refused(404));
    }
    let id = segments[0];
    if !crate::tenant::valid_library_name(id) {
        return Some(super::bad_request(format!(
            "{id:?} is not a usable flow name: letters, digits, - and _ only"
        )));
    }
    Some(match (method, segments.as_slice()) {
        ("GET", [_]) => get_flow(root, id, request),
        ("POST", [_]) => save_flow(root, id, &request.body),
        ("GET", [_, _]) => match flowlib::versions(root, id) {
            Ok(versions) => Response::json(200, &serde_json::json!({ "versions": versions })),
            Err(e) => server_error(e),
        },
        ("DELETE", [_, _, n]) => delete_version(root, id, n),
        _ => Response::refused(404),
    })
}

fn server_error(e: String) -> Response {
    Response::json(500, &serde_json::json!({ "error": e }))
}

/// `?version=N` is optional; present but not a whole number is a bad request.
fn get_flow(root: &std::path::Path, id: &str, request: &Request) -> Response {
    let version = match request.query("version") {
        None => None,
        Some(v) => match v.parse::<u32>() {
            Ok(n) => Some(n),
            Err(_) => return super::bad_request(format!("version {v:?} is not a whole number")),
        },
    };
    match flowlib::load(root, id, version) {
        Ok(Some(flow)) => match serde_json::to_value(&flow) {
            Ok(value) => Response::json(200, &value),
            Err(e) => server_error(e.to_string()),
        },
        Ok(None) => {
            let error = match version {
                Some(v) => format!("no version {v} of flow {id}"),
                None => format!("no flow named {id}"),
            };
            Response::json(404, &serde_json::json!({ "error": error }))
        }
        Err(e) => server_error(e),
    }
}

/// Mirrors the old engine's save: check, match the path id, then demand a version bump.
fn save_flow(root: &std::path::Path, id: &str, body: &[u8]) -> Response {
    let value: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return super::bad_request(format!("the body is not JSON: {e}")),
    };
    let report = check_flow(&value);
    if !report.errors.is_empty() {
        return Response::json(
            422,
            &serde_json::json!({ "errors": report.errors, "warnings": report.warnings }),
        );
    }
    // check_flow has already proved this deserializes; the error arm is only a guard.
    let flow: FlowDef = match serde_json::from_value(value) {
        Ok(f) => f,
        Err(e) => {
            return Response::json(
                422,
                &serde_json::json!({ "errors": [e.to_string()], "warnings": report.warnings }),
            )
        }
    };
    if flow.id != id {
        return Response::json(
            422,
            &serde_json::json!({
                "errors": [format!("body id `{}` does not match path id `{id}`", flow.id)],
                "warnings": report.warnings,
            }),
        );
    }
    let latest = match flowlib::versions(root, id) {
        Ok(versions) => versions.into_iter().max(),
        Err(e) => return server_error(e),
    };
    if let Some(max) = latest {
        if flow.version <= max {
            return Response::json(
                409,
                &serde_json::json!({
                    "error": format!(
                        "version {} is not above the stored latest {max}; a save must bump the version, so an old version is never rewritten",
                        flow.version
                    ),
                    "latest_version": max,
                }),
            );
        }
    }
    match flowlib::put(root, &flow) {
        Ok(_) => Response::json(
            200,
            &serde_json::json!({
                "saved": { "id": flow.id, "version": flow.version },
                "warnings": report.warnings,
                "note": "saved; restart ns-app for companies running this flow to pick it up",
            }),
        ),
        Err(e) => server_error(e),
    }
}

/// Rolls back one version. The last remaining version is refused, and so is a version that does not exist.
fn delete_version(root: &std::path::Path, id: &str, n: &str) -> Response {
    let version: u32 = match n.parse() {
        Ok(v) => v,
        Err(_) => return super::bad_request(format!("version {n:?} is not a whole number")),
    };
    let versions = match flowlib::versions(root, id) {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    if !versions.contains(&version) {
        return Response::json(
            404,
            &serde_json::json!({ "error": format!("no version {version} of flow {id}") }),
        );
    }
    if versions.len() == 1 {
        return Response::json(
            409,
            &serde_json::json!({ "error": format!(
                "version {version} is the only version of {id}; delete the flow's directory by hand if you mean to remove it, after taking it off every company that names it"
            ) }),
        );
    }
    match flowlib::delete(root, id, version) {
        Ok(()) => {
            let latest = versions.iter().copied().filter(|v| *v != version).max();
            Response::json(
                200,
                &serde_json::json!({
                    "deleted": { "id": id, "version": version },
                    "latest_version": latest,
                }),
            )
        }
        Err(e) => server_error(e),
    }
}

/// What a save would refuse (errors) and what it would only mention (warnings).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Report {
    pub(super) errors: Vec<String>,
    pub(super) warnings: Vec<String>,
}

/// Every top-level key `FlowDef` deserializes; anything else is silently
/// dropped by serde and would be lost on the next save — which is exactly why
/// it is worth a warning.
const KNOWN_KEYS: &[&str] = &[
    "id",
    "version",
    "roles",
    "required_roles",
    "actions",
    "fast_path",
    "reply_template",
    "templates",
    "follow_up_ms",
];

pub(super) fn check_flow(body: &serde_json::Value) -> Report {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if let Some(map) = body.as_object() {
        for key in map.keys() {
            if !KNOWN_KEYS.contains(&key.as_str()) {
                warnings.push(format!(
                    "unknown field `{key}` will be dropped on save — the engine does not read it"
                ));
            }
        }
    }

    let flow: FlowDef = match serde_json::from_value(body.clone()) {
        Ok(f) => f,
        Err(e) => {
            errors.push(format!("not a valid flow document: {e}"));
            return Report { errors, warnings };
        }
    };

    for e in flow.validate() {
        errors.push(e.to_string());
    }

    // Template ids are never checked by the engine until reply time, where a
    // missing one fails the turn — catch it at save time instead. The field
    // carries a small grammar: "" is silent, "$out:action.field" takes the
    // reply from an action's output, anything else names a template.
    if let Some(t) = &flow.reply_template {
        if let Some(rest) = t.strip_prefix("$out:") {
            match rest.split_once('.') {
                Some((action, field)) if !action.is_empty() && !field.is_empty() => {
                    if !flow.actions.iter().any(|a| a.name.as_str() == action) {
                        errors.push(format!(
                            "reply_template reads `$out:{action}.{field}`, but no action is named `{action}`"
                        ));
                    }
                }
                _ => errors.push(format!(
                    "reply_template `{t}` is a malformed $out ref — the shape is $out:action.field"
                )),
            }
        } else if !t.is_empty() && !flow.templates.contains_key(t) {
            errors.push(format!(
                "reply_template names `{t}`, which is not in templates"
            ));
        }
    }
    for (i, rule) in flow.fast_path.iter().enumerate() {
        let t = &rule.reply_template;
        if t.starts_with("$out:") {
            // The fast path runs no actions, so there is no output to read.
            errors.push(format!(
                "fast_path[{i}] uses `{t}`: the fast path runs zero actions, so an action-output \
                 reply is unreachable there — name a template or use \"\" for silence"
            ));
        } else if !t.is_empty() && !flow.templates.contains_key(t) {
            errors.push(format!(
                "fast_path[{i}] names template `{t}`, which is not in templates"
            ));
        }
    }

    // Advisory only: what v2 keeps but does not act on yet.
    for action in &flow.actions {
        if let Guard::IntentIs(i) = &action.when {
            warnings.push(format!(
                "action `{}` waits for intent `{i}`: v2 has no intent classifier yet, so it is never offered",
                action.name
            ));
        }
    }
    if !flow.fast_path.is_empty() {
        warnings.push(
            "fast_path is kept but v2 does not run it yet: these rules answer nothing".into(),
        );
    }
    if flow
        .reply_template
        .as_deref()
        .is_some_and(|t| !t.is_empty())
        || !flow.templates.is_empty()
    {
        warnings.push(
            "reply_template and templates are kept but v2 does not render them yet: the model writes every reply"
                .into(),
        );
    }
    if flow.follow_up_ms.is_some() {
        warnings.push("follow_up_ms is kept but v2 does not wake conversations yet".into());
    }
    for action in &flow.actions {
        if !action.reads.is_empty() || !action.writes.is_empty() || !action.args.is_empty() {
            warnings.push(format!(
                "action `{}`: reads, writes and args are checked but v2 does not run them yet; the model fills the tool's arguments",
                action.name
            ));
        }
    }
    for action in &flow.actions {
        warnings.push(format!(
            "action `{}` offers tool `{}`: the company must have a tool by that name (its own, a module's, or a built-in one), or nothing is offered",
            action.name, action.component
        ));
    }

    Report { errors, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HELLO: &str = include_str!("../../../crates/core/tests/fixtures/flows/hello.json");
    const TRIP: &str = include_str!("../../../crates/core/tests/fixtures/flows/trip_test.json");

    #[test]
    fn a_minimal_flow_is_clean() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":null});
        let report = check_flow(&body);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn the_v1_fixtures_have_no_errors() {
        let hello: serde_json::Value = serde_json::from_str(HELLO).unwrap();
        let report = check_flow(&hello);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.contains(
            &"fast_path is kept but v2 does not run it yet: these rules answer nothing".to_string()
        ));
        assert!(report.warnings.contains(
            &"reply_template and templates are kept but v2 does not render them yet: the model writes every reply".to_string()
        ));

        let trip: serde_json::Value = serde_json::from_str(TRIP).unwrap();
        let report = check_flow(&trip);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    #[test]
    fn an_unknown_key_warns() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":null,"colour":1});
        let report = check_flow(&body);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("unknown field `colour`")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn not_a_flow_is_one_error() {
        let report = check_flow(&json!({"id": 3}));
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(report.errors[0].starts_with("not a valid flow document"));
    }

    #[test]
    fn validate_errors_are_reported() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":["ghost"],"actions":[],"fast_path":[],"reply_template":null});
        let report = check_flow(&body);
        assert_eq!(
            report.errors,
            vec!["flow does not define role ghost".to_string()]
        );
    }

    #[test]
    fn reply_template_must_name_a_template() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":"nope"});
        let report = check_flow(&body);
        assert!(
            report.errors.iter().any(|e| e.contains("not in templates")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn reply_template_out_ref_needs_its_action() {
        let missing = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":"$out:fetch.text"});
        let report = check_flow(&missing);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("no action is named `fetch`")),
            "{:?}",
            report.errors
        );

        let malformed = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"fast_path":[],"reply_template":"$out:bad"});
        let report = check_flow(&malformed);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("malformed $out ref")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn fast_path_cannot_reply_from_an_action() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"actions":[],"reply_template":null,
            "fast_path":[{"match_on":null,"pattern":"hi","sets":{},"reply_template":"$out:a.b"}]});
        let report = check_flow(&body);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.starts_with("fast_path[0] uses")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn intent_guard_warns() {
        let body = json!({"id":"f","version":1,"roles":{},"required_roles":[],"fast_path":[],"reply_template":null,
            "actions":[{"name":"buy","component":"shop","reads":[],"writes":[],"write_scope":"conversation","args":{},
                "when":{"when":"intent_is","value":"purchase"}}]});
        let report = check_flow(&body);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.contains(
            &"action `buy` waits for intent `purchase`: v2 has no intent classifier yet, so it is never offered".to_string()
        ));
        assert!(report.warnings.contains(
            &"action `buy` offers tool `shop`: the company must have a tool by that name (its own, a module's, or a built-in one), or nothing is offered".to_string()
        ));
        assert!(!report
            .warnings
            .iter()
            .any(|w| w.contains("reads, writes and args are checked")));
    }

    use std::path::Path;
    use std::sync::Arc;

    fn admin(root: &Path) -> Arc<super::super::Admin> {
        Arc::new(super::super::Admin {
            root: root.to_path_buf(),
            token: "the-token".into(),
        })
    }

    fn request(method: &str, path: &str, token: Option<&str>, body: &str) -> Request {
        Request {
            method: method.into(),
            target: path.into(),
            headers: token
                .map(|t| vec![("authorization".into(), format!("Bearer {t}"))])
                .unwrap_or_default(),
            body: body.as_bytes().to_vec(),
            keep_alive: false,
        }
    }

    async fn call(
        admin: &Arc<super::super::Admin>,
        method: &str,
        path: &str,
        body: &str,
    ) -> Response {
        super::super::route(admin, &request(method, path, Some("the-token"), body)).await
    }

    fn json_of(response: &Response) -> serde_json::Value {
        serde_json::from_slice(&response.body).expect("a JSON body")
    }

    fn flow(id: &str, version: u32) -> String {
        json!({"id": id, "version": version, "roles": {}, "required_roles": [], "actions": [], "fast_path": [], "reply_template": null}).to_string()
    }

    const GHOST: &str = r#"{"id":"trip","version":1,"roles":{},"required_roles":["ghost"],"actions":[],"fast_path":[],"reply_template":null}"#;

    #[tokio::test]
    async fn listing_is_empty_then_shows_a_saved_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        let r = call(&a, "GET", "/api/flows", "").await;
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r), json!({"flows": []}));

        let r = call(&a, "POST", "/api/flows/trip", &flow("trip", 1)).await;
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r)["saved"]["version"], 1);

        let r = call(&a, "GET", "/api/flows", "").await;
        assert_eq!(
            json_of(&r),
            json!({"flows": [{"id": "trip", "latest_version": 1}]})
        );
    }

    #[tokio::test]
    async fn a_save_must_bump_the_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        assert_eq!(
            call(&a, "POST", "/api/flows/trip", &flow("trip", 1))
                .await
                .status,
            200
        );

        let r = call(&a, "POST", "/api/flows/trip", &flow("trip", 1)).await;
        assert_eq!(r.status, 409);
        assert_eq!(json_of(&r)["latest_version"], 1);

        assert_eq!(
            call(&a, "POST", "/api/flows/trip", &flow("trip", 2))
                .await
                .status,
            200
        );

        let r = call(&a, "GET", "/api/flows/trip", "").await;
        assert_eq!(json_of(&r)["version"], 2);
        let r = call(&a, "GET", "/api/flows/trip?version=1", "").await;
        assert_eq!(json_of(&r)["version"], 1);
        let r = call(&a, "GET", "/api/flows/trip/versions", "").await;
        assert_eq!(json_of(&r), json!({"versions": [1, 2]}));
    }

    #[tokio::test]
    async fn an_invalid_flow_is_422_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        let r = call(&a, "POST", "/api/flows/trip", GHOST).await;
        assert_eq!(r.status, 422);
        let errors = json_of(&r)["errors"].to_string();
        assert!(errors.contains("ghost"), "{errors}");
        assert!(!dir.path().join("flows").join("trip").exists());
    }

    #[tokio::test]
    async fn a_mismatched_id_is_422() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        let r = call(&a, "POST", "/api/flows/trip", &flow("other", 1)).await;
        assert_eq!(r.status, 422);
    }

    #[tokio::test]
    async fn delete_rolls_back_but_never_the_last() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        assert_eq!(
            call(&a, "POST", "/api/flows/trip", &flow("trip", 1))
                .await
                .status,
            200
        );
        assert_eq!(
            call(&a, "POST", "/api/flows/trip", &flow("trip", 2))
                .await
                .status,
            200
        );

        let r = call(&a, "DELETE", "/api/flows/trip/versions/2", "").await;
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r)["latest_version"], 1);

        let r = call(&a, "DELETE", "/api/flows/trip/versions/1", "").await;
        assert_eq!(r.status, 409);

        let r = call(&a, "DELETE", "/api/flows/trip/versions/7", "").await;
        assert_eq!(r.status, 404);
    }

    #[tokio::test]
    async fn unknown_flow_is_404() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        let r = call(&a, "GET", "/api/flows/nope", "").await;
        assert_eq!(r.status, 404);
    }

    #[tokio::test]
    async fn a_bad_id_is_400() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        let r = call(&a, "GET", "/api/flows/..%2Fx", "").await;
        assert_eq!(r.status, 400);
        let r = call(&a, "POST", "/api/flows/a.b", &flow("a.b", 1)).await;
        assert_eq!(r.status, 400);
    }

    #[tokio::test]
    async fn validate_reports_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        let r = call(&a, "POST", "/api/validate-flow", GHOST).await;
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r)["ok"], false);
        assert!(!dir.path().join("flows").exists());
    }

    #[tokio::test]
    async fn flow_routes_need_the_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = admin(dir.path());
        let r = super::super::route(&a, &request("GET", "/api/flows", None, "")).await;
        assert_eq!(r.status, 401);
    }
}
