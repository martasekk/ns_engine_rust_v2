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
mod library;
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
        ("GET", "/api/vault") => admin.vault(),
        ("GET", "/api/library") => admin.library(),
        ("POST", "/api/library") => admin.save_library(&request.body),
        ("POST", "/api/group") => admin.save_group(&request.body),
        ("POST", "/api/settings") => admin.save_settings(&request.body),
        ("POST", "/api/company") => admin.add_company(&request.body),
        ("POST", "/api/company/rename") => refuse_rename(),
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
struct GroupRequest {
    company: String,
    name: String,
    #[serde(default)]
    modules: Vec<String>,
    /// Take it out rather than write it.
    #[serde(default)]
    remove: bool,
}

#[derive(serde::Deserialize)]
struct LibraryRequest {
    /// "persona" or "module".
    kind: String,
    name: String,
    #[serde(default)]
    text: String,
    /// This is a new thing, not an edit. A name already taken is refused
    /// rather than blanked.
    #[serde(default)]
    fresh: bool,
}

/// Everything read off disk for one request. Built once per call so the
/// form and the vault are always describing the same files.
struct Survey {
    companies: Vec<String>,
    /// The process's own config, which is a reader of the library too.
    base: AppConfig,
    /// Each company's overlay as parsed on its own — what says which
    /// library things it *names*, which the merged config has already
    /// resolved away.
    configs: Vec<(String, AppConfig)>,
    summaries: Vec<CompanySummary>,
    process_values: serde_json::Value,
    company_values: serde_json::Value,
    named: Vec<secrets::Reference>,
}

/// One row of the company list: what an operator needs to see without
/// opening anything — who it is, what it runs on, how a message reaches it,
/// and whether it can work at all yet.
#[derive(serde::Serialize)]
struct CompanySummary {
    id: String,
    /// The model this company's emitter runs.
    model: String,
    /// Whether that model is its own choice or the process's. A page that
    /// showed only the model would make every company look configured.
    own_model: bool,
    /// The ways in it is reachable by. Empty means nothing can reach it —
    /// which is a normal state for a company added a minute ago, and not
    /// one to leave silent.
    reach: Vec<String>,
    /// How many of its variables have no value. The one number that says
    /// "this company will not work yet".
    missing: usize,
    /// Why its overlay does not load, when it does not. A row that showed a
    /// plausible model for a company the loader refuses would be the page
    /// disagreeing with startup, which is the one thing it is here to
    /// prevent.
    problem: Option<String>,
}

/// The model this config's emitter resolves to, or the reason it does not.
/// The reason is the one startup would print, so an operator reading the row
/// is reading the error they would otherwise have met at the next restart.
fn emitter_model(llm: &crate::config::LlmConfig) -> Result<String, String> {
    llm.roles()?
        .into_iter()
        .find(|target| target.role == crate::config::Role::Emitter)
        .map(|target| target.model)
        .ok_or_else(|| "no emitter role resolves".to_string())
}

/// The ways a message can arrive for this company. Both are properties of
/// what it holds, never of a route it was given: there is one socket and one
/// endpoint for the whole process, and a company is told apart by the
/// credential or by the account id inside a signed payload.
fn reachable_by(overlay: &AppConfig) -> Vec<String> {
    let mut ways = Vec::new();
    if !overlay.auth.signing_key_envs.is_empty() {
        ways.push("a token it signs".to_string());
    }
    if overlay
        .whatsapp
        .as_ref()
        .is_some_and(|w| !w.phone_number_id.trim().is_empty())
    {
        ways.push("WhatsApp".to_string());
    }
    ways
}

/// This company's variables that have no value yet.
fn missing_values(
    id: &str,
    named: &[secrets::Reference],
    entries: &[secrets::SecretStatus],
) -> usize {
    let mut theirs: Vec<&str> = Vec::new();
    for reference in named.iter().filter(|r| r.company.as_deref() == Some(id)) {
        if !theirs.contains(&reference.name.as_str()) {
            theirs.push(&reference.name);
        }
    }
    theirs
        .iter()
        .filter(|name| !entries.iter().any(|e| e.name == **name && e.set))
        .count()
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
        let survey = match self.survey() {
            Ok(survey) => survey,
            Err(e) => return Response::json(200, &serde_json::json!({ "error": e })),
        };
        Response::json(
            200,
            &serde_json::json!({
                "root": self.root.display().to_string(),
                "process": { "sections": schema::PROCESS, "values": survey.process_values },
                "company": { "sections": schema::COMPANY, "values": survey.company_values },
                "companies": survey.companies,
                "summaries": survey.summaries,
                // Per company, name → the modules that group may reach.
                // Nothing enforces it yet; the page says so.
                "groups": survey.configs.iter()
                    .map(|(id, c)| (id.clone(), serde_json::json!(c.groups)))
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                "secrets": secrets::status(&self.root, &survey.named),
                "secretsFile": secrets::SECRETS_FILE,
            }),
        )
    }

    /// The vault: every variable the configuration names, and every one the
    /// file holds that nothing names yet. Values are not part of it — the
    /// route exists to say what is set and who reaches for it.
    fn vault(&self) -> Response {
        let survey = match self.survey() {
            Ok(survey) => survey,
            Err(e) => return Response::json(200, &serde_json::json!({ "error": e })),
        };
        Response::json(
            200,
            &serde_json::json!({
                "entries": secrets::vault(&self.root, &survey.named),
                "secretsFile": secrets::SECRETS_FILE,
            }),
        )
    }

    /// The shared library: every persona and module, each carrying the
    /// companies that name it. The readers travel with the thing because
    /// the page has to name them before an edit, not after.
    fn library(&self) -> Response {
        let survey = match self.survey() {
            Ok(survey) => survey,
            Err(e) => return Response::json(200, &serde_json::json!({ "error": e })),
        };
        Response::json(
            200,
            &serde_json::json!({
                "entries": library::list(&self.root, &survey.base, &survey.configs),
            }),
        )
    }

    fn save_library(&self, body: &[u8]) -> Response {
        let Ok(request) = serde_json::from_slice::<LibraryRequest>(body) else {
            return Response::refused(400);
        };
        let Some(kind) = library::Kind::parse(&request.kind) else {
            return bad_request(format!(
                "{:?} is not something this library holds — a persona or a module",
                request.kind
            ));
        };
        match library::write(
            &self.root,
            kind,
            request.name.trim(),
            &request.text,
            request.fresh,
        ) {
            Ok(()) => Response::json(
                200,
                &serde_json::json!({ "saved": request.name, "kind": request.kind }),
            ),
            Err(e) => bad_request(e),
        }
    }

    /// One group, written or removed.
    ///
    /// Groups do not go through [`schema`]'s allowlist because their names
    /// are the operator's, and a fixed list of dotted paths cannot hold a
    /// name nobody has chosen yet. The property the allowlist exists for is
    /// kept another way: this writes under `groups.` and nowhere else, and
    /// the name is checked against the same charset a library name is, so a
    /// caller cannot walk out of the table with a dotted name.
    fn save_group(&self, body: &[u8]) -> Response {
        let Ok(request) = serde_json::from_slice::<GroupRequest>(body) else {
            return Response::refused(400);
        };
        let id = request.company.trim();
        if !self.company_ids().iter().any(|known| known == id) {
            return bad_request(format!("no company {id:?} is configured"));
        }
        let name = request.name.trim();
        if !library::valid_name(name) {
            return bad_request(format!(
                "{name:?} is not usable as a group name: letters, digits, dashes and \
                 underscores — a dot would make it a table of its own"
            ));
        }
        let path = self.company_path(id);
        let mut doc = match document::read(&path) {
            Ok(doc) => doc,
            Err(e) => return bad_request(e.to_string()),
        };
        let setting = if request.remove {
            Setting::Unset
        } else {
            // A group with no modules is a real thing to say — these people
            // reach nothing — so it is written as an empty list rather than
            // collapsing into a removal the way a cleared field does.
            Setting::List(request.modules.clone())
        };
        if let Err(e) = document::set(&mut doc, &format!("groups.{name}"), setting) {
            return bad_request(e.to_string());
        }
        if let Err(e) = document::loads_as_config(&doc) {
            return bad_request(e.to_string());
        }
        if let Err(e) = document::write(&path, &doc) {
            return bad_request(e.to_string());
        }
        Response::json(200, &serde_json::json!({ "saved": name }))
    }

    /// The base config, every company's overlay, and every variable the
    /// whole set names — what both `/api/state` and `/api/vault` are built
    /// from, read once so the two can never disagree.
    fn survey(&self) -> Result<Survey, String> {
        // The base config as startup reads it, so the page reports what is
        // in force rather than what the file says in isolation.
        let base_text = std::fs::read_to_string(self.config_path()).unwrap_or_default();
        let base = AppConfig::parse(&base_text).map_err(|e| e.to_string())?;
        let companies = self.company_ids();
        let mut configs = Vec::new();
        let mut company_values = serde_json::Map::new();
        for id in &companies {
            let doc = document::read(&self.company_path(id)).map_err(|e| e.to_string())?;
            company_values.insert(id.clone(), values_of(&doc, schema::COMPANY));
            if let Ok(cfg) = AppConfig::parse(&doc.to_string()) {
                configs.push((id.clone(), cfg));
            }
        }
        let doc = document::read(&self.config_path()).map_err(|e| e.to_string())?;
        // Deliberately the overlays *alone*: a merged config inherits the
        // base's key variable, which would make every company reference the
        // house key and make it look shared the moment there were two.
        let named = schema::variables_named(&base, &configs);
        let entries = secrets::status(&self.root, &named);
        let base_model = emitter_model(&base.llm).ok();
        // Every company gets a row, including one whose overlay does not
        // parse. That company is exactly the one somebody needs to open and
        // fix, and a list built only from the overlays that loaded would
        // leave it with no row to click.
        let summaries = companies
            .iter()
            .map(|id| {
                if !configs.iter().any(|(known, _)| known == id) {
                    return CompanySummary {
                        id: id.clone(),
                        model: "—".to_string(),
                        own_model: false,
                        reach: Vec::new(),
                        missing: 0,
                        problem: Some(format!(
                            "{}.toml is not a configuration this can read — open it and \
                             fix the line it names at startup",
                            id
                        )),
                    };
                }
                // The model and the ways in are properties of the merged
                // config, because that is the one the company's engine is
                // built from. Running the loader's own merge is what keeps
                // this row and startup from ever disagreeing.
                let merged = crate::tenant::load_one(&base_text, &self.root, id);
                let (model, problem) = match &merged {
                    Ok(tenant) => match emitter_model(&tenant.app.llm) {
                        Ok(model) => (model, None),
                        Err(why) => ("—".to_string(), Some(why)),
                    },
                    Err(e) => ("—".to_string(), Some(e.to_string())),
                };
                CompanySummary {
                    id: id.clone(),
                    // Its own model means the merged config runs something
                    // other than what the process runs — which is true
                    // whether it named the model or named a provider that
                    // carried one. Reading the overlay for a `model` key
                    // would call a company inherited while it ran a model
                    // the process has never heard of.
                    own_model: problem.is_none() && Some(&model) != base_model.as_ref(),
                    model,
                    reach: match &merged {
                        Ok(tenant) => reachable_by(&tenant.app),
                        Err(_) => Vec::new(),
                    },
                    missing: missing_values(id, &named, &entries),
                    problem,
                }
            })
            .collect();
        Ok(Survey {
            process_values: values_of(&doc, schema::PROCESS),
            company_values: serde_json::Value::Object(company_values),
            companies,
            base,
            configs,
            summaries,
            named,
        })
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
            // A library field's *value* is a file name, which makes it the
            // one place on this page where an accepted key can still carry
            // a path. The loader refuses it too — this is so the operator
            // hears about it now rather than at the next start.
            if let Err(e) = library_names_are_usable(field, &setting) {
                return bad_request(e);
            }
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
        // Removing a key one company names breaks that company, and the
        // operator doing it is looking at its row. Removing one that *two*
        // companies name breaks a company that is not on the screen, so it
        // is refused and both are named: the fix is to give one of them its
        // own entry first.
        //
        // A survey that cannot be built is a refusal, not a licence: if one
        // tenant file is unreadable this cannot tell whether the key is
        // shared, and guessing "it is not" is the guess that breaks a
        // company.
        let survey = match self.survey() {
            Ok(survey) => survey,
            Err(e) => {
                return bad_request(format!(
                    "cannot tell who uses {} while the configuration does not read ({e}), \
                     so it is left alone",
                    request.name
                ))
            }
        };
        let sharing = secrets::owners_referencing(&survey.named, &request.name);
        if sharing.len() > 1 {
            return bad_request(format!(
                "{} is the key {} {} use — removing it would stop the one you are not \
                 looking at. Give one of them its own variable first.",
                request.name,
                sharing.join(" and "),
                if sharing.len() == 2 { "both" } else { "all" }
            ));
        }
        match secrets::remove(&self.root, &request.name) {
            Ok(()) => Response::json(200, &serde_json::json!({ "removed": request.name })),
            Err(e) => bad_request(e),
        }
    }
}

/// A `[library]` field carries a name that becomes a file name, so its
/// value is checked the way the loader checks it. Every other field's value
/// is only ever a value.
fn library_names_are_usable(field: &schema::Field, setting: &Setting) -> Result<(), String> {
    let names: Vec<&String> = match (field.kind, setting) {
        (schema::Kind::PersonaRef, Setting::Text(name)) => vec![name],
        (schema::Kind::ModuleRefs, Setting::List(names)) => names.iter().collect(),
        _ => return Ok(()),
    };
    for name in names {
        if !library::valid_name(name) {
            return Err(format!(
                "{name:?} is not usable as a library name: letters, digits, dashes and \
                 underscores, because it names a file under personas/ or modules/"
            ));
        }
    }
    Ok(())
}

fn bad_request(detail: String) -> Response {
    Response::json(400, &serde_json::json!({ "error": detail }))
}

/// H3. A company's id is the first segment of every session id it has ever
/// had and the name of its store file, so a rename would leave its history
/// addressed by a name nothing answers to. The route exists only to say
/// that in words: the page does not offer the control, and a caller that
/// went looking for it gets the reason rather than a bare 404.
///
/// Copying is not offered either — it would be the same id problem with an
/// extra database, and the two companies would share every variable the
/// copy inherited without either page saying so.
fn refuse_rename() -> Response {
    bad_request(
        "a company cannot be renamed: its id is the first segment of every session id it \
         has ever had and the name of its store file, and neither of those follows. Add \
         the new company and move the conversations deliberately, or keep the id and \
         change what it is called on its persona."
            .to_string(),
    )
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
        Kind::List | Kind::ModuleRefs => {
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
        Kind::Text | Kind::Paragraph | Kind::EnvName | Kind::PersonaRef => {
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
            ("GET", "/api/vault"),
            ("GET", "/api/library"),
            ("POST", "/api/library"),
            ("POST", "/api/settings"),
            ("POST", "/api/secret"),
            ("POST", "/api/secret/remove"),
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

        // T1.3 / H2: the vault is a second way to ask the same question,
        // and the rule has to hold on it too or the feature undid the test.
        let vault = route(&admin, &request("GET", "/api/vault", Some("the-token"), "")).await;
        let body = String::from_utf8_lossy(&vault.body);
        assert!(
            !body.contains("sk-do-not-echo-me"),
            "a secret reached the page through the vault: {body}"
        );
        assert!(body.contains("NS_TEST_ADMIN_STATE_KEY"), "{body}");
        std::env::remove_var("NS_TEST_ADMIN_STATE_KEY");
    }

    /// Two overlays, one `[llm] api_key_env`.
    fn two_companies_on_one_key(root: &Path) {
        std::fs::write(
            root.join("config.toml"),
            "[llm]\nprovider = \"openrouter\"\napi_key_env = \"NS_TEST_ADMIN_HOUSE\"\n",
        )
        .expect("the config");
        std::fs::create_dir_all(root.join("tenants")).expect("tenants");
        for id in ["acme", "beta"] {
            std::fs::write(
                root.join("tenants").join(format!("{id}.toml")),
                format!(
                    "[store]\npath = \"ns-{id}.sqlite\"\n\
                     [llm]\nprovider = \"groq\"\napi_key_env = \"NS_TEST_ADMIN_BOTH\"\n"
                ),
            )
            .expect("the overlay");
        }
    }

    /// T1.4 / H1. Two companies naming one key share the provider's quota
    /// and, by the throttle's key of (base URL, key hash), one throttle.
    /// The row names both, so it is never something to discover later.
    #[tokio::test]
    async fn a_key_two_companies_reference_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        two_companies_on_one_key(dir.path());
        let admin = admin(dir.path());

        let vault = route(&admin, &request("GET", "/api/vault", Some("the-token"), "")).await;
        assert_eq!(vault.status, 200);
        let answered: serde_json::Value =
            serde_json::from_slice(&vault.body).expect("the vault is json");
        let entries = answered["entries"].as_array().expect("entries");

        let both = entries
            .iter()
            .find(|e| e["name"] == "NS_TEST_ADMIN_BOTH")
            .expect("the shared entry");
        assert_eq!(both["shared"], true, "{both}");
        assert_eq!(both["companies"], serde_json::json!(["acme", "beta"]));

        // The process's own key is named by nobody's overlay, so it is not
        // shared however many companies the deployment has.
        let house = entries
            .iter()
            .find(|e| e["name"] == "NS_TEST_ADMIN_HOUSE")
            .expect("the process key");
        assert_eq!(house["shared"], false, "{house}");
    }

    /// T1.2. Removing a key one company names is its own business; removing
    /// one two companies name breaks the company that is not on the screen.
    #[tokio::test]
    async fn removing_a_key_two_companies_reference_is_refused_naming_both() {
        let dir = tempfile::tempdir().expect("tempdir");
        two_companies_on_one_key(dir.path());
        let admin = admin(dir.path());
        secrets::set(dir.path(), "NS_TEST_ADMIN_BOTH", "shared-value").expect("sets");
        secrets::set(dir.path(), "NS_TEST_ADMIN_HOUSE", "house-value").expect("sets");

        let refused = route(
            &admin,
            &request(
                "POST",
                "/api/secret/remove",
                Some("the-token"),
                r#"{"name":"NS_TEST_ADMIN_BOTH"}"#,
            ),
        )
        .await;
        assert_eq!(refused.status, 400);
        let said = String::from_utf8_lossy(&refused.body);
        assert!(said.contains("acme") && said.contains("beta"), "{said}");
        let text = std::fs::read_to_string(dir.path().join(secrets::SECRETS_FILE)).expect("reads");
        assert!(text.contains("NS_TEST_ADMIN_BOTH"), "still there: {text}");

        // The one only the process names is removable as it always was.
        let removed = route(
            &admin,
            &request(
                "POST",
                "/api/secret/remove",
                Some("the-token"),
                r#"{"name":"NS_TEST_ADMIN_HOUSE"}"#,
            ),
        )
        .await;
        assert_eq!(removed.status, 200);
        std::env::remove_var("NS_TEST_ADMIN_BOTH");
        std::env::remove_var("NS_TEST_ADMIN_HOUSE");
    }

    /// T2.2. The config layer always allowed a company its own model; only
    /// the page did not offer it. This is that gap closed, through the
    /// route that would refuse a path the schema does not own.
    #[tokio::test]
    async fn a_company_can_be_given_its_own_model_and_its_own_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"openrouter\"\nemitter = { model = \"house/model\" }\n",
        )
        .expect("the config");
        let admin = admin(dir.path());
        route(
            &admin,
            &request(
                "POST",
                "/api/company",
                Some("the-token"),
                r#"{"id":"acme"}"#,
            ),
        )
        .await;

        let saved = route(
            &admin,
            &request(
                "POST",
                "/api/settings",
                Some("the-token"),
                r#"{"company":"acme","values":{"llm.provider":"groq",
                    "llm.api_key_env":"NS_LLM_API_KEY_ACME",
                    "llm.emitter.model":"theirs/model"}}"#,
            ),
        )
        .await;
        assert_eq!(
            saved.status,
            200,
            "{}",
            String::from_utf8_lossy(&saved.body)
        );

        let text = std::fs::read_to_string(dir.path().join("tenants").join("acme.toml"))
            .expect("the overlay");
        assert!(text.contains("theirs/model"), "{text}");
        assert!(text.contains("NS_LLM_API_KEY_ACME"), "{text}");

        // And the list says it is theirs rather than the house's.
        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        let acme = &answered["summaries"][0];
        assert_eq!(acme["id"], "acme");
        assert_eq!(acme["model"], "theirs/model");
        assert_eq!(acme["own_model"], true);
        // Its signing key and its provider key, neither of them set yet.
        assert_eq!(acme["missing"], 2, "{acme}");
        assert_eq!(acme["reach"], serde_json::json!(["a token it signs"]));
    }

    /// A company that asks for nothing of its own runs the process's model,
    /// and the row says so rather than making it look chosen.
    #[tokio::test]
    async fn a_company_without_its_own_model_shows_the_process_one_as_inherited() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"openrouter\"\nemitter = { model = \"house/model\" }\n",
        )
        .expect("the config");
        let admin = admin(dir.path());
        route(
            &admin,
            &request(
                "POST",
                "/api/company",
                Some("the-token"),
                r#"{"id":"acme"}"#,
            ),
        )
        .await;

        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        let acme = &answered["summaries"][0];
        assert_eq!(acme["model"], "house/model");
        assert_eq!(acme["own_model"], false, "{acme}");
    }

    /// An overlay the loader refuses is reported as refused, rather than
    /// shown with a plausible model it will never run. The page and startup
    /// disagreeing is the failure this whole module exists to avoid.
    #[tokio::test]
    async fn a_company_whose_overlay_the_loader_refuses_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"openrouter\"\nemitter = { model = \"house/model\" }\n",
        )
        .expect("the config");
        std::fs::create_dir_all(dir.path().join("tenants")).expect("tenants");
        // `http` is process-owned: a company that could move the endpoint
        // could take another company's callers.
        std::fs::write(
            dir.path().join("tenants").join("acme.toml"),
            "[http]\nlisten = \"0.0.0.0:9\"\n",
        )
        .expect("the overlay");
        let admin = admin(dir.path());

        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        let acme = &answered["summaries"][0];
        assert!(acme["problem"].is_string(), "{acme}");
        assert!(
            acme["problem"].as_str().expect("text").contains("http"),
            "{acme}"
        );
        assert_eq!(acme["reach"], serde_json::json!([]));
    }

    /// T4.1. A group is a name and a set of modules, written into the
    /// company's own overlay and read back from it.
    #[tokio::test]
    async fn a_group_is_stored_on_the_company_and_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"ollama\"\n",
        )
        .expect("the config");
        let admin = admin(dir.path());
        route(
            &admin,
            &request(
                "POST",
                "/api/company",
                Some("the-token"),
                r#"{"id":"acme"}"#,
            ),
        )
        .await;

        let saved = route(
            &admin,
            &request(
                "POST",
                "/api/group",
                Some("the-token"),
                r#"{"company":"acme","name":"agents","modules":["orders","stock"]}"#,
            ),
        )
        .await;
        assert_eq!(
            saved.status,
            200,
            "{}",
            String::from_utf8_lossy(&saved.body)
        );

        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        assert_eq!(
            answered["groups"]["acme"]["agents"],
            serde_json::json!(["orders", "stock"])
        );

        // A group that reaches nothing is a real thing to say, so an empty
        // set is written rather than collapsing into a removal.
        route(
            &admin,
            &request(
                "POST",
                "/api/group",
                Some("the-token"),
                r#"{"company":"acme","name":"agents","modules":[]}"#,
            ),
        )
        .await;
        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        assert_eq!(answered["groups"]["acme"]["agents"], serde_json::json!([]));

        // And removing takes it out.
        route(
            &admin,
            &request(
                "POST",
                "/api/group",
                Some("the-token"),
                r#"{"company":"acme","name":"agents","remove":true}"#,
            ),
        )
        .await;
        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        assert_eq!(answered["groups"]["acme"], serde_json::json!({}));
    }

    /// The groups route is the one place that writes outside the schema's
    /// allowlist, so it keeps the property the allowlist exists for by
    /// hand: under `groups.` and nowhere else.
    #[tokio::test]
    async fn a_group_name_cannot_walk_out_of_its_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        route(
            &admin,
            &request(
                "POST",
                "/api/company",
                Some("the-token"),
                r#"{"id":"acme"}"#,
            ),
        )
        .await;
        for bad in ["serve.listen", "../x", "has space", ""] {
            let body = serde_json::json!({ "company": "acme", "name": bad, "modules": [] });
            let refused = route(
                &admin,
                &request("POST", "/api/group", Some("the-token"), &body.to_string()),
            )
            .await;
            assert_eq!(refused.status, 400, "{bad:?} was accepted");
        }
        // And a company that does not exist is not a file to create.
        let body = r#"{"company":"nobody","name":"agents","modules":[]}"#;
        let refused = route(
            &admin,
            &request("POST", "/api/group", Some("the-token"), body),
        )
        .await;
        assert_eq!(refused.status, 400);
    }

    /// H6, the half a test can hold: the page says in as many words that
    /// nothing is enforced, and names the plan that owns the enforcement.
    #[test]
    fn the_page_says_groups_are_not_enforced_and_which_plan_owns_it() {
        assert!(PAGE.contains("Nothing enforces this yet"), "the notice");
        assert!(PAGE.contains("Phase 8"), "the plan that owns enforcement");
        assert!(PAGE.contains("grants"), "the seam it will arrive through");
    }

    /// T3.4 / H4, through the routes: a persona two companies name comes
    /// back carrying both, so the page can say so before the save.
    #[tokio::test]
    async fn a_shared_persona_reports_every_company_it_reaches() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"ollama\"\n",
        )
        .expect("the config");
        std::fs::create_dir_all(dir.path().join("tenants")).expect("tenants");
        for (id, extra) in [("acme", ""), ("beta", "[persona]\ntext = \"ours\"\n")] {
            std::fs::write(
                dir.path().join("tenants").join(format!("{id}.toml")),
                format!("[library]\npersona = \"support-brief\"\n{extra}"),
            )
            .expect("the overlay");
        }
        let admin = admin(dir.path());

        let created = route(
            &admin,
            &request(
                "POST",
                "/api/library",
                Some("the-token"),
                r#"{"kind":"persona","name":"support-brief","text":"Be brief."}"#,
            ),
        )
        .await;
        assert_eq!(created.status, 200);

        let listed = route(
            &admin,
            &request("GET", "/api/library", Some("the-token"), ""),
        )
        .await;
        let answered: serde_json::Value = serde_json::from_slice(&listed.body).expect("json");
        let entry = &answered["entries"][0];
        assert_eq!(entry["name"], "support-brief");
        assert_eq!(entry["used_by"], serde_json::json!(["acme", "beta"]));
        // beta writes its own, so the shared text never reaches it.
        assert_eq!(entry["overridden_by"], serde_json::json!(["beta"]));
    }

    /// The one field on this page whose accepted key can still carry a
    /// path. The loader refuses it too; this is so the operator hears about
    /// it at the save rather than at the next start.
    #[tokio::test]
    async fn a_library_reference_that_would_leave_the_directory_is_refused_at_the_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        route(
            &admin,
            &request(
                "POST",
                "/api/company",
                Some("the-token"),
                r#"{"id":"acme"}"#,
            ),
        )
        .await;

        for body in [
            r#"{"company":"acme","values":{"library.persona":"../../../secrets"}}"#,
            r#"{"company":"acme","values":{"library.modules":["ok","../escape"]}}"#,
        ] {
            let refused = route(
                &admin,
                &request("POST", "/api/settings", Some("the-token"), body),
            )
            .await;
            assert_eq!(refused.status, 400, "{body}");
        }
        let text = std::fs::read_to_string(dir.path().join("tenants").join("acme.toml"))
            .expect("the overlay");
        assert!(!text.contains(".."), "nothing was written: {text}");
    }

    /// A module is checked at the save, where the operator is still looking
    /// at it, rather than at the next start.
    #[tokio::test]
    async fn a_module_that_does_not_parse_is_refused_at_the_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        let refused = route(
            &admin,
            &request(
                "POST",
                "/api/library",
                Some("the-token"),
                r#"{"kind":"module","name":"orders","text":"not [[[ toml"}"#,
            ),
        )
        .await;
        assert_eq!(refused.status, 400);
        assert!(!dir.path().join("modules").exists(), "nothing was written");

        // And a name that would leave the directory is refused too.
        let escape = route(
            &admin,
            &request(
                "POST",
                "/api/library",
                Some("the-token"),
                r#"{"kind":"persona","name":"../../escape","text":"x"}"#,
            ),
        )
        .await;
        assert_eq!(escape.status, 400);
    }

    /// A company whose overlay is not a configuration at all still gets a
    /// row. It is the one somebody has to open and fix, and a list built
    /// only from the overlays that loaded would leave it with nothing to
    /// click and no hint that it exists.
    #[tokio::test]
    async fn a_company_whose_overlay_does_not_parse_still_has_a_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"openrouter\"\n",
        )
        .expect("the config");
        std::fs::create_dir_all(dir.path().join("tenants")).expect("tenants");
        // Valid TOML, and not a configuration.
        std::fs::write(
            dir.path().join("tenants").join("beta.toml"),
            "[llm]\nprovider = 5\n",
        )
        .expect("the overlay");
        let admin = admin(dir.path());

        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        let rows = answered["summaries"].as_array().expect("summaries");
        assert_eq!(
            rows.len(),
            1,
            "the broken company still has a row: {rows:?}"
        );
        assert_eq!(rows[0]["id"], "beta");
        assert!(rows[0]["problem"].is_string(), "{}", rows[0]);
    }

    /// A company that names a provider and no model runs that provider's
    /// default, which is not the process's model. Calling it inherited
    /// would print a caption naming a model the process does not run.
    #[tokio::test]
    async fn a_company_that_names_only_a_provider_is_not_called_inherited() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"openrouter\"\n",
        )
        .expect("the config");
        std::fs::create_dir_all(dir.path().join("tenants")).expect("tenants");
        std::fs::write(
            dir.path().join("tenants").join("acme.toml"),
            "[llm]\nprovider = \"mistral\"\n",
        )
        .expect("the overlay");
        let admin = admin(dir.path());

        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let answered: serde_json::Value = serde_json::from_slice(&state.body).expect("json");
        let acme = &answered["summaries"][0];
        assert_eq!(acme["model"], "mistral-small-latest");
        assert_eq!(acme["own_model"], true, "{acme}");
    }

    /// T2.4 / H3. The id is the first segment of every session id the
    /// company has and the name of its store file. Neither follows a
    /// rename, so there is no rename — and a caller that looks for one is
    /// told why rather than given a 404 to guess at.
    #[tokio::test]
    async fn a_company_cannot_be_renamed_and_the_refusal_says_why() {
        let dir = tempfile::tempdir().expect("tempdir");
        let admin = admin(dir.path());
        let refused = route(
            &admin,
            &request(
                "POST",
                "/api/company/rename",
                Some("the-token"),
                r#"{"id":"acme","to":"acme-gmbh"}"#,
            ),
        )
        .await;
        assert_eq!(refused.status, 400);
        let said = String::from_utf8_lossy(&refused.body);
        assert!(said.contains("session id"), "{said}");
        assert!(said.contains("store file"), "{said}");
        // Not a setting either, so it cannot arrive through a save.
        assert!(schema::field_of(schema::COMPANY, "id").is_none());
    }

    /// A key minted before the company that will use it is still in the
    /// vault, and still absent from the form's own list.
    #[tokio::test]
    async fn a_key_nothing_references_is_in_the_vault_and_not_in_the_form() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.toml"),
            "[llm]\nprovider = \"ollama\"\n",
        )
        .expect("the config");
        let admin = admin(dir.path());
        secrets::set(dir.path(), "NS_TEST_ADMIN_EARLY", "minted-early").expect("sets");

        let vault = route(&admin, &request("GET", "/api/vault", Some("the-token"), "")).await;
        let body = String::from_utf8_lossy(&vault.body);
        assert!(body.contains("NS_TEST_ADMIN_EARLY"), "{body}");
        assert!(!body.contains("minted-early"), "a value reached the page");

        let state = route(&admin, &request("GET", "/api/state", Some("the-token"), "")).await;
        let state = String::from_utf8_lossy(&state.body);
        assert!(
            !state.contains("NS_TEST_ADMIN_EARLY"),
            "the form drew a variable nothing names: {state}"
        );
        std::env::remove_var("NS_TEST_ADMIN_EARLY");
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
