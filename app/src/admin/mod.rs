//! `ns-app admin`: a page for setting this deployment up.
//!
//! What it is for is the hour before anything works — choosing a provider,
//! naming the variable its key lives in, putting the key there, adding a
//! company, giving that company a signing key — done in a form instead of in
//! four files and a shell. It is a *setup* tool, not an operations console:
//! it shows no conversations, no logs and no spend.
//!
//! # Two rules it does not bend
//!
//! **A secret never goes into the config.** The config names variables, and
//! that is what keeps a key out of a file somebody commits. The page writes
//! *values* to a `.env` beside it ([`secrets`]) and leaves the naming where
//! it was.
//!
//! **A secret never goes to the browser.** The page is told a variable's
//! name, what the config uses it for, and whether it is set. Never the
//! value. A settings page that renders keys puts them in the scrollback and
//! the cache of everyone who opens it, and there is no reason it ever needs
//! to read one back.
//!
//! # Who may reach it
//!
//! Loopback only, with no flag to say otherwise — unlike the channels, which
//! have `allow_remote` for an operator who meant it. This one edits
//! credentials and has no TLS, so "meant it" is not a thing it accepts.
//! Beyond that it is one printed token per run, presented as a bearer
//! credential on every API call, because "on loopback" is not an
//! authorisation argument on a machine with more than one user or a browser
//! that runs other people's javascript.
//!
//! The token is minted per run and never stored, so closing the process ends
//! every session it had.
//!
//! # Map
//!
//! | module       | what it owns                                        |
//! |--------------|-----------------------------------------------------|
//! | [`document`] | changing one key in a TOML file, comments and all   |
//! | [`schema`]   | which keys the page may set, and what each means    |
//! | [`secrets`]  | the values the config only names                    |
//! | this file    | the socket, the routes, and who may call them       |

mod document;
mod schema;
pub(crate) mod secrets;

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nschannel_http::http::{read_request, Request, Response};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};

use crate::config::AppConfig;
use document::Setting;

/// The page itself, compiled in: a setup tool that cannot find its own
/// assets is worse than one file that is bigger than it looks.
const PAGE: &str = include_str!("page.html");

/// The most a save may be. A persona is prose; nothing here is a file
/// upload.
const MAX_BODY: usize = 256 * 1024;

pub(crate) struct Admin {
    /// The directory holding `config.toml` and `tenants/`. Everything this
    /// page reads or writes is under it.
    root: PathBuf,
    token: String,
}

/// Serves until the process is stopped. Prints the one URL that works.
pub(crate) async fn run(root: PathBuf, listen: &str) -> Result<(), String> {
    let addr: SocketAddr = listen
        .parse()
        .map_err(|_| format!("{listen:?} is not a host:port"))?;
    // No `allow_remote` here, deliberately: this page edits credentials and
    // has no TLS. An operator who wants it from another machine has an SSH
    // tunnel, which is the tool for that and is already on their box.
    if !addr.ip().is_loopback() {
        return Err(format!(
            "refusing to serve the settings page on {addr}: it edits credentials and has \
             no TLS, so it is loopback only — forward a port instead"
        ));
    }
    let token = one_time_token();
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("{addr}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    // The token is in the fragment, which is never sent with a request and
    // so reaches no log; the page reads it there and presents it as a bearer
    // credential on each call.
    println!("Settings: http://{bound}/#token={token}");
    println!("(loopback only, and this link ends when this process does)");

    let admin = Arc::new(Admin { root, token });
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            return Err("the settings listener stopped accepting".into());
        };
        let admin = admin.clone();
        tokio::spawn(async move {
            connection(&admin, stream, peer).await;
        });
    }
}

/// 128 bits of the OS's randomness, hex. Per run, never written down.
fn one_time_token() -> String {
    // The workspace has no rng dependency and this needs one number. The
    // OS's own source is what a dependency would have called anyway; on
    // Windows and Unix alike this file is the interface to it.
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        // No /dev/urandom (Windows): the address of a fresh allocation, the
        // clock and the process id, hashed. Not a key — a token for a
        // loopback page that lives as long as one process.
        let seed = format!(
            "{:p}{}{}",
            Box::into_raw(Box::new(0u8)),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        bytes.copy_from_slice(&nsidentity::hmac_sha256(seed.as_bytes(), b"admin")[..16]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn connection(admin: &Arc<Admin>, stream: TcpStream, peer: SocketAddr) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    loop {
        let request = match read_request(&mut reader, MAX_BODY).await {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(e) => {
                let _ = Response::refused(e.status()).write_to(&mut w, false).await;
                return;
            }
        };
        let keep_alive = request.keep_alive;
        let response = route(admin, &request).await;
        if response.write_to(&mut w, keep_alive).await.is_err() || !keep_alive {
            return;
        }
        let _ = peer;
    }
}

async fn route(admin: &Arc<Admin>, request: &Request) -> Response {
    // The page carries no data, so it is served before anything is checked;
    // everything it then asks for is not.
    if request.method == "GET" && (request.path() == "/" || request.path() == "/index.html") {
        return Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            headers: Vec::new(),
            body: PAGE.as_bytes().to_vec(),
        };
    }
    if !authorised(admin, request) {
        return Response::refused(401);
    }
    match (request.method.as_str(), request.path()) {
        ("GET", "/api/state") => admin.state(),
        ("POST", "/api/settings") => admin.save_settings(&request.body),
        ("POST", "/api/company") => admin.add_company(&request.body),
        ("POST", "/api/secret") => admin.save_secret(&request.body),
        ("POST", "/api/secret/remove") => admin.remove_secret(&request.body),
        _ => Response::refused(404),
    }
}

/// Constant-time, because a comparison that stops at the first wrong byte
/// tells a local script how much of the token it has guessed.
fn authorised(admin: &Arc<Admin>, request: &Request) -> bool {
    let Some(value) = request.header("authorization") else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ").map(str::trim) else {
        return false;
    };
    let (a, b) = (presented.as_bytes(), admin.token.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(serde::Deserialize)]
struct SaveRequest {
    /// The company this is for, or absent for the process itself.
    #[serde(default)]
    company: Option<String>,
    values: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct CompanyRequest {
    id: String,
}

#[derive(serde::Deserialize)]
struct SecretRequest {
    name: String,
    #[serde(default)]
    value: String,
}

impl Admin {
    fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    fn company_path(&self, id: &str) -> PathBuf {
        self.root
            .join(crate::tenant::TENANT_DIR)
            .join(format!("{id}.toml"))
    }

    /// Everything the page draws itself from, in one call: the two field
    /// schemas, the current value of each, the companies, and which
    /// variables are set.
    fn state(&self) -> Response {
        let base = match self.load_base() {
            Ok(base) => base,
            Err(e) => return Response::json(200, &serde_json::json!({ "error": e })),
        };
        let companies = self.company_ids();
        let mut company_configs = Vec::new();
        let mut company_values = serde_json::Map::new();
        for id in &companies {
            let doc = match document::read(&self.company_path(id)) {
                Ok(doc) => doc,
                Err(e) => {
                    return Response::json(200, &serde_json::json!({ "error": e.to_string() }))
                }
            };
            company_values.insert(id.clone(), values_of(&doc, schema::COMPANY));
            if let Ok(cfg) = AppConfig::parse(&doc.to_string()) {
                company_configs.push((id.clone(), cfg));
            }
        }
        let doc = match document::read(&self.config_path()) {
            Ok(doc) => doc,
            Err(e) => return Response::json(200, &serde_json::json!({ "error": e.to_string() })),
        };
        let named = schema::variables_named(&base, &company_configs);
        Response::json(
            200,
            &serde_json::json!({
                "root": self.root.display().to_string(),
                "process": { "sections": schema::PROCESS, "values": values_of(&doc, schema::PROCESS) },
                "company": { "sections": schema::COMPANY, "values": company_values },
                "companies": companies,
                "secrets": secrets::status(&self.root, &named),
                "secretsFile": secrets::SECRETS_FILE,
            }),
        )
    }

    /// The base config as startup reads it, so the page reports what is in
    /// force rather than what the file says in isolation.
    fn load_base(&self) -> Result<AppConfig, String> {
        let text = std::fs::read_to_string(self.config_path()).unwrap_or_default();
        AppConfig::parse(&text).map_err(|e| e.to_string())
    }

    fn company_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(self.root.join(crate::tenant::TENANT_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension()? == "toml")
                    .then(|| path.file_stem()?.to_str().map(str::to_string))?
            })
            .collect();
        ids.sort();
        ids
    }

    fn save_settings(&self, body: &[u8]) -> Response {
        let Ok(save) = serde_json::from_slice::<SaveRequest>(body) else {
            return Response::refused(400);
        };
        let (sections, path) = match &save.company {
            None => (schema::PROCESS, self.config_path()),
            Some(id) => {
                if !self.company_ids().contains(id) {
                    return bad_request(format!("no company {id:?} is configured"));
                }
                (schema::COMPANY, self.company_path(id))
            }
        };
        let mut doc = match document::read(&path) {
            Ok(doc) => doc,
            Err(e) => return bad_request(e.to_string()),
        };
        for (field_path, value) in &save.values {
            // The schema is the allowlist, not a hint: a path nobody drew a
            // control for is a caller writing keys of its own choosing.
            let Some(field) = schema::field_of(sections, field_path) else {
                return bad_request(format!("{field_path} is not a setting this page owns"));
            };
            let setting = match to_setting(field, value) {
                Ok(setting) => setting,
                Err(e) => return bad_request(e),
            };
            if let Err(e) = document::set(&mut doc, field_path, setting) {
                return bad_request(e.to_string());
            }
        }
        // The gate: what is written must load, or the next start would fail
        // on a file this page wrote.
        if let Err(e) = document::loads_as_config(&doc) {
            return bad_request(e.to_string());
        }
        if let Err(e) = document::write(&path, &doc) {
            return bad_request(e.to_string());
        }
        Response::json(
            200,
            &serde_json::json!({ "saved": path.display().to_string() }),
        )
    }

    /// A new company is a new overlay with its own database, which is the
    /// one field it cannot sensibly default to the same as everybody's.
    fn add_company(&self, body: &[u8]) -> Response {
        let Ok(request) = serde_json::from_slice::<CompanyRequest>(body) else {
            return Response::refused(400);
        };
        let id = request.id.trim().to_string();
        // The id becomes the first segment of every session this company
        // ever has, so it takes the charset a session segment may use.
        if !nsidentity::valid_claim(&id) {
            return bad_request(format!(
                "{id:?} is not usable as a company id: 1-64 characters of [A-Za-z0-9_.:-], \
                 because it becomes the first segment of every session id"
            ));
        }
        let path = self.company_path(&id);
        if path.exists() {
            return bad_request(format!("{id} already has an overlay at {}", path.display()));
        }
        let mut doc = toml_edit::DocumentMut::new();
        // Its own store, named for it: two companies on one file is refused
        // at startup naming both, and defaulting them all to `ns.sqlite`
        // would make that the normal first experience.
        let _ = document::set(
            &mut doc,
            "store.path",
            Setting::Text(format!("ns-{id}.sqlite")),
        );
        let _ = document::set(
            &mut doc,
            "auth.signing_key_envs",
            Setting::List(vec![format!("NS_SIGNING_KEY_{}", env_suffix(&id))]),
        );
        if let Err(e) = document::write(&path, &doc) {
            return bad_request(e.to_string());
        }
        Response::json(
            200,
            &serde_json::json!({ "created": id, "path": path.display().to_string() }),
        )
    }

    fn save_secret(&self, body: &[u8]) -> Response {
        let Ok(request) = serde_json::from_slice::<SecretRequest>(body) else {
            return Response::refused(400);
        };
        if request.value.is_empty() {
            return bad_request("a value was not given".to_string());
        }
        match secrets::set(&self.root, &request.name, &request.value) {
            // Deliberately nothing about the value, not even its length.
            Ok(()) => Response::json(200, &serde_json::json!({ "set": request.name })),
            Err(e) => bad_request(e),
        }
    }

    fn remove_secret(&self, body: &[u8]) -> Response {
        let Ok(request) = serde_json::from_slice::<SecretRequest>(body) else {
            return Response::refused(400);
        };
        match secrets::remove(&self.root, &request.name) {
            Ok(()) => Response::json(200, &serde_json::json!({ "removed": request.name })),
            Err(e) => bad_request(e),
        }
    }
}

fn bad_request(detail: String) -> Response {
    Response::json(400, &serde_json::json!({ "error": detail }))
}

fn env_suffix(id: &str) -> String {
    id.to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// One section's worth of current values, for the page to fill its form in.
fn values_of(doc: &toml_edit::DocumentMut, sections: &[schema::Section]) -> serde_json::Value {
    let mut values = serde_json::Map::new();
    for section in sections {
        for field in section.fields {
            let value = match document::get(doc, field.path) {
                Some(Setting::Text(s)) => serde_json::Value::String(s),
                Some(Setting::Number(n)) => serde_json::Value::from(n),
                Some(Setting::Flag(b)) => serde_json::Value::Bool(b),
                Some(Setting::List(items)) => serde_json::Value::from(items),
                Some(Setting::Unset) | None => serde_json::Value::Null,
            };
            values.insert(field.path.to_string(), value);
        }
    }
    serde_json::Value::Object(values)
}

/// What the page sent, as the field says it must be. A number that arrived
/// as text is parsed here rather than written as a string, because the
/// config would then refuse to load and the page would blame the save.
fn to_setting(field: &schema::Field, value: &serde_json::Value) -> Result<Setting, String> {
    use schema::Kind;
    // Empty means "leave it to the default", which is a removal rather than
    // an empty string.
    if value.is_null() || value.as_str().is_some_and(|s| s.trim().is_empty()) {
        return Ok(Setting::Unset);
    }
    Ok(match field.kind {
        Kind::Number => {
            let n = match value {
                serde_json::Value::Number(n) => n.as_i64(),
                serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            };
            Setting::Number(n.ok_or_else(|| format!("{} takes a number", field.label))?)
        }
        Kind::Flag => Setting::Flag(match value {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => s == "true",
            _ => return Err(format!("{} is on or off", field.label)),
        }),
        Kind::List => {
            let items: Vec<String> = match value {
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect(),
                serde_json::Value::String(s) => s
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect(),
                _ => return Err(format!("{} takes a list", field.label)),
            };
            if items.is_empty() {
                return Ok(Setting::Unset);
            }
            Setting::List(items)
        }
        Kind::Choice => {
            let chosen = value.as_str().unwrap_or_default().to_string();
            if !field.choices.contains(&chosen.as_str()) {
                return Err(format!(
                    "{} is one of: {}",
                    field.label,
                    field.choices.join(", ")
                ));
            }
            Setting::Text(chosen)
        }
        Kind::Text | Kind::Paragraph | Kind::EnvName => {
            Setting::Text(value.as_str().unwrap_or_default().to_string())
        }
    })
}

/// Adds `.env` to `.gitignore` if it is not already covered, so the first
/// secret written is not the one that gets committed. Silent if there is no
/// `.gitignore`: a directory that is not a repository needs no line.
pub(crate) fn keep_secrets_out_of_git(root: &Path) {
    let path = root.join(".gitignore");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let covered = text
        .lines()
        .map(str::trim)
        .any(|line| line == secrets::SECRETS_FILE || line == "/.env" || line == ".env*");
    if covered {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&path) {
        let _ = writeln!(
            file,
            "\n# Values for the variables the config names, written by `ns-app admin`.\n/{}",
            secrets::SECRETS_FILE
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin(root: &Path) -> Arc<Admin> {
        Arc::new(Admin {
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

    #[tokio::test]
    async fn every_api_call_needs_the_token_and_the_page_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        // The page carries no data of its own.
        assert_eq!(
            route(&admin, &request("GET", "/", None, "")).await.status,
            200
        );
        for (method, path) in [
            ("GET", "/api/state"),
            ("POST", "/api/settings"),
            ("POST", "/api/secret"),
            ("POST", "/api/company"),
        ] {
            let refused = route(&admin, &request(method, path, None, "{}")).await;
            assert_eq!(refused.status, 401, "{method} {path}");
            let wrong = route(&admin, &request(method, path, Some("guess"), "{}")).await;
            assert_eq!(wrong.status, 401, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn a_save_writes_the_named_key_and_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "# chosen for a reason\n[llm]\nprovider = \"ollama\"\n\n[serve]\nlisten = \"127.0.0.1:7375\"\n",
        )
        .expect("the config");
        let admin = admin(dir.path());

        let saved = route(
            &admin,
            &request(
                "POST",
                "/api/settings",
                Some("the-token"),
                r#"{"values":{"llm.provider":"groq"}}"#,
            ),
        )
        .await;
        assert_eq!(saved.status, 200);

        let text = std::fs::read_to_string(dir.path().join("config.toml")).expect("reads");
        assert!(text.contains("provider = \"groq\""), "{text}");
        assert!(text.contains("# chosen for a reason"), "{text}");
        assert!(text.contains("listen = \"127.0.0.1:7375\""), "{text}");
    }

    /// The allowlist is the schema. A caller naming a key nobody drew a
    /// control for is refused rather than obliged.
    #[tokio::test]
    async fn a_key_the_page_does_not_own_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        let refused = route(
            &admin,
            &request(
                "POST",
                "/api/settings",
                Some("the-token"),
                r#"{"values":{"store.path_of_my_choosing":"/etc/passwd"}}"#,
            ),
        )
        .await;
        assert_eq!(refused.status, 400);
        assert!(
            !dir.path().join("config.toml").exists(),
            "nothing was written"
        );
    }

    /// The gate, through the API: a value that would stop the process from
    /// starting is refused, and the file on disk is untouched.
    #[tokio::test]
    async fn a_save_that_would_not_load_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let before = "[serve]\nmax_connections = 8\n";
        std::fs::write(dir.path().join("config.toml"), before).expect("the config");
        let admin = admin(dir.path());

        let refused = route(
            &admin,
            &request(
                "POST",
                "/api/settings",
                Some("the-token"),
                r#"{"values":{"serve.max_connections":"lots"}}"#,
            ),
        )
        .await;
        assert_eq!(refused.status, 400);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).expect("reads"),
            before,
            "the file on disk is what it was"
        );
    }

    #[tokio::test]
    async fn a_new_company_gets_its_own_store_and_its_own_key_variable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        let created = route(
            &admin,
            &request(
                "POST",
                "/api/company",
                Some("the-token"),
                r#"{"id":"acme"}"#,
            ),
        )
        .await;
        assert_eq!(created.status, 200);

        let text = std::fs::read_to_string(dir.path().join("tenants").join("acme.toml"))
            .expect("the overlay");
        // Two companies on one database is refused at startup naming both,
        // so a new one never starts out sharing.
        assert!(text.contains("ns-acme.sqlite"), "{text}");
        assert!(text.contains("NS_SIGNING_KEY_ACME"), "{text}");

        // And the same id twice is refused rather than overwriting.
        let again = route(
            &admin,
            &request(
                "POST",
                "/api/company",
                Some("the-token"),
                r#"{"id":"acme"}"#,
            ),
        )
        .await;
        assert_eq!(again.status, 400);
    }

    #[tokio::test]
    async fn a_company_id_that_could_not_be_a_session_segment_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        for bad in ["a/b", "", "with space", "../escape"] {
            let body = serde_json::json!({ "id": bad }).to_string();
            let refused = route(
                &admin,
                &request("POST", "/api/company", Some("the-token"), &body),
            )
            .await;
            assert_eq!(refused.status, 400, "{bad:?} was accepted");
        }
    }

    /// The rule the whole module exists to keep: what the page is told
    /// about a secret can be rendered anywhere.
    #[tokio::test]
    async fn no_secret_value_is_ever_in_what_the_page_receives() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"openrouter\"\napi_key_env = \"NS_TEST_ADMIN_STATE_KEY\"\n",
        )
        .expect("the config");
        let admin = admin(dir.path());

        let set = route(
            &admin,
            &request(
                "POST",
                "/api/secret",
                Some("the-token"),
                r#"{"name":"NS_TEST_ADMIN_STATE_KEY","value":"sk-do-not-echo-me"}"#,
            ),
        )
        .await;
        assert_eq!(set.status, 200);
        assert!(!String::from_utf8_lossy(&set.body).contains("sk-do-not-echo-me"));

        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let body = String::from_utf8_lossy(&state.body);
        assert!(
            !body.contains("sk-do-not-echo-me"),
            "a secret reached the page: {body}"
        );
        // It is reported as set, which is the whole of what the page needs.
        assert!(body.contains("NS_TEST_ADMIN_STATE_KEY"), "{body}");
        assert!(body.contains("\"set\":true"), "{body}");
        std::env::remove_var("NS_TEST_ADMIN_STATE_KEY");
    }

    #[test]
    fn the_secrets_file_is_added_to_gitignore_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".gitignore");
        std::fs::write(&path, "/target\n").expect("the file");

        keep_secrets_out_of_git(dir.path());
        let text = std::fs::read_to_string(&path).expect("reads");
        assert!(text.contains("/.env"), "{text}");

        keep_secrets_out_of_git(dir.path());
        let again = std::fs::read_to_string(&path).expect("reads");
        assert_eq!(
            again.matches("/.env").count(),
            1,
            "added once, not once per run"
        );
    }

    #[test]
    fn a_directory_that_is_not_a_repository_gets_no_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        keep_secrets_out_of_git(dir.path());
        assert!(!dir.path().join(".gitignore").exists());
    }
}
