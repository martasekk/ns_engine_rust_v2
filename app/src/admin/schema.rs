//! What the page may change, and what each field means.
//!
//! A curated list rather than "edit any key you like". Three reasons, and
//! the third is the one that matters: a named list can be rendered as a form
//! with the right control and the right help text; it can be validated per
//! field instead of per file; and it cannot be used to write a key the
//! config does not read, which is how a settings page ends up with settings
//! that quietly do nothing.
//!
//! A field is a dotted path into the document ([`super::document`]), so
//! adding one is a line here and nothing else.

use super::secrets::Reference;
use crate::config::AppConfig;

/// Which control the page draws, and what the value must parse as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Text,
    /// A longer text: the persona is paragraphs, not a line.
    Paragraph,
    Number,
    Flag,
    /// One of a fixed set.
    Choice,
    /// A list of strings, one per line in the page.
    List,
    /// Text, and the *name of a variable* rather than a value. Rendered
    /// beside whether that variable is set, because naming a variable
    /// nobody exported is the likeliest mistake on this page.
    EnvName,
    /// The name of one persona in the shared library. A choice, but over a
    /// set that is on disk rather than in this file, so it cannot be a
    /// [`Kind::Choice`] with its fixed list.
    PersonaRef,
    /// Names of modules in the shared library. Several, and they add to
    /// whatever the company defines inline rather than replacing it.
    ModuleRefs,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Field {
    /// The dotted path this writes, e.g. `llm.emitter.model`.
    pub(crate) path: &'static str,
    pub(crate) label: &'static str,
    pub(crate) kind: Kind,
    /// Shown under the control. Says what the setting *does*, not what it
    /// is called.
    pub(crate) help: &'static str,
    /// For [`Kind::Choice`].
    pub(crate) choices: &'static [&'static str],
    /// What the config does when this is left empty.
    pub(crate) default: &'static str,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Section {
    pub(crate) title: &'static str,
    pub(crate) note: &'static str,
    pub(crate) fields: &'static [Field],
}

const fn field(
    path: &'static str,
    label: &'static str,
    kind: Kind,
    help: &'static str,
    default: &'static str,
) -> Field {
    Field {
        path,
        label,
        kind,
        help,
        choices: &[],
        default,
    }
}

const fn choice(
    path: &'static str,
    label: &'static str,
    choices: &'static [&'static str],
    help: &'static str,
    default: &'static str,
) -> Field {
    Field {
        path,
        label,
        kind: Kind::Choice,
        help,
        choices,
        default,
    }
}

/// `config.toml`: what the process owns. Everything here is the same for
/// every company it hosts, which is why none of it is in an overlay.
pub(crate) const PROCESS: &[Section] = &[
    Section {
        title: "Model",
        note: "Which provider and models this deployment drives. The key itself is never \
               here — the config names a variable, and the Vault is where that variable \
               gets its value.",
        fields: &[
            field(
                "llm.provider",
                "Provider",
                Kind::Text,
                "A preset name: openrouter, ollama, lmstudio, groq, openai, mistral, \
                 llamacpp. It supplies the endpoint, the key's variable name, a default \
                 model and the provider's rate limit.",
                "openrouter",
            ),
            field(
                "llm.base_url",
                "Endpoint",
                Kind::Text,
                "Spelled out, when the provider is not one of the presets. Anything \
                 speaking OpenAI-style /v1/chat/completions with function tools works.",
                "the preset's",
            ),
            field(
                "llm.api_key_env",
                "API key variable",
                Kind::EnvName,
                "The name of the variable holding the provider key — not the key.",
                "the preset's",
            ),
            field(
                "llm.emitter.model",
                "Model",
                Kind::Text,
                "The model that chooses the actions and writes the reply. There were two \
                 of these — one to act and one to narrate what happened — and they are \
                 one call now, so this is the choice that decides how good the agent is.",
                "the provider's default",
            ),
        ],
    },
    Section {
        title: "Persona and memory",
        note: "The base persona, and how much of a conversation is kept verbatim. A \
               company can override the persona in its own settings.",
        fields: &[
            field(
                "persona.text",
                "Persona",
                Kind::Paragraph,
                "What the agent is, in its own words. Every turn carries it.",
                "empty",
            ),
            field(
                "store.path",
                "Database",
                Kind::Text,
                "The event log, which is the only history the engine has. A company with \
                 its own store path uses that instead.",
                "ns.sqlite",
            ),
            field(
                "memory.window_turns",
                "Turns kept verbatim",
                Kind::Number,
                "How many recent turns ride on every call word for word. Older ones are \
                 folded into a rolling summary.",
                "6",
            ),
        ],
    },
    Section {
        title: "The socket",
        note: "One JSON object per line: what a desktop app or a script connects to.",
        fields: &[
            field(
                "serve.listen",
                "Listen on",
                Kind::Text,
                "host:port. Loopback unless you also allow remote.",
                "127.0.0.1:7375",
            ),
            choice(
                "serve.auth",
                "How a client proves who it is",
                &["shared", "jwt"],
                "`shared` is one token for everyone and proves nothing, so it is refused \
                 off loopback and cannot serve more than one company. `jwt` is a signed \
                 token per person, verified against each company's own keys.",
                "shared",
            ),
            field(
                "serve.token_env",
                "Shared token variable",
                Kind::EnvName,
                "Only read under `shared`. The variable holding the one token every \
                 client presents.",
                "NS_SERVE_TOKEN",
            ),
            field(
                "serve.max_connections",
                "Connections at once",
                Kind::Number,
                "Machine-wide. The next one is closed as it is accepted.",
                "8",
            ),
            field(
                "serve.allow_remote",
                "Allow a non-loopback address",
                Kind::Flag,
                "Off, a bind to anything but loopback is refused at startup. There is no \
                 TLS here: terminate it in front.",
                "off",
            ),
        ],
    },
    Section {
        title: "The HTTP endpoint",
        note: "Web chat windows, one-shot requests, and every platform webhook. Leave the \
               address empty and it is never opened.",
        fields: &[
            field(
                "http.listen",
                "Listen on",
                Kind::Text,
                "host:port, or empty for not at all.",
                "not opened",
            ),
            field(
                "http.origins",
                "Browser origins allowed",
                Kind::List,
                "One per line. `*` for a widget embedded on customer sites whose domains \
                 you do not know. Empty means no cross-origin page may open a window.",
                "none",
            ),
            field(
                "http.reply_timeout_ms",
                "Wait for a reply (ms)",
                Kind::Number,
                "How long POST /v1/messages holds a request open before it answers 504. \
                 The turn keeps running either way.",
                "120000",
            ),
            field(
                "http.max_connections",
                "Connections at once",
                Kind::Number,
                "",
                "256",
            ),
            field(
                "http.allow_remote",
                "Allow a non-loopback address",
                Kind::Flag,
                "Same rule as the socket, for the same reason.",
                "off",
            ),
            field(
                "http.seen_path",
                "Answered-messages file",
                Kind::Text,
                "Where platform message ids are remembered, so a webhook retry after a \
                 restart is still a retry and not a second turn. Empty keeps them in \
                 memory only.",
                "ns-webhooks-seen.tsv",
            ),
            field(
                "http.whatsapp_verify_token_env",
                "WhatsApp verify token variable",
                Kind::EnvName,
                "The variable holding the token Meta echoes during its one-off \
                 verification. One per process, because the endpoint is.",
                "unset",
            ),
        ],
    },
];

/// `tenants/<id>.toml`: what one company owns. The process-owned keys are
/// deliberately absent — a company that could move the listen address could
/// take another company's callers, and the loader refuses an overlay that
/// sets one.
pub(crate) const COMPANY: &[Section] = &[
    Section {
        title: "Their model",
        note: "Left empty, this company runs on the process's provider, key and model. \
               Filling any of it in gives them their own — including their own quota, if \
               the key is theirs alone. The models catalogue stays the process's.",
        fields: &[
            field(
                "llm.provider",
                "Provider",
                Kind::Text,
                "A preset name, as on the Process tab. Naming one here points this \
                 company's calls somewhere else entirely, key included.",
                "the process's",
            ),
            field(
                "llm.base_url",
                "Endpoint",
                Kind::Text,
                "Spelled out, when the provider is not one of the presets.",
                "the process's",
            ),
            field(
                "llm.api_key_env",
                "API key variable",
                Kind::EnvName,
                "The name of the variable holding this company's provider key — not the \
                 key. A variable another company also names is a shared quota and a \
                 shared throttle, and the Vault tab says which.",
                "the process's",
            ),
            field(
                "llm.emitter.model",
                "Model",
                Kind::Text,
                "The model that chooses this company's actions and writes its replies. \
                 One model does both, so this is the choice that decides how good their \
                 agent is.",
                "the process's",
            ),
        ],
    },
    Section {
        title: "From the library",
        note: "Shared things, named rather than copied — so a correction to one persona \
               reaches every company using it. Editing one of these on the Library tab \
               changes it for all of them, and that tab says which.",
        fields: &[
            field(
                "library.persona",
                "Shared persona",
                Kind::PersonaRef,
                "One of personas/. A company that writes its own persona below gets that \
                 instead, and the reference sits unused.",
                "none",
            ),
            field(
                "library.modules",
                "Shared modules",
                Kind::ModuleRefs,
                "Each is a file of tools in modules/. They add to whatever this company \
                 defines for itself — this company also has X, rather than only X.",
                "none",
            ),
        ],
    },
    Section {
        title: "Who they are",
        note: "Anything left empty falls back to the process settings.",
        fields: &[
            field(
                "persona.text",
                "Persona",
                Kind::Paragraph,
                "This company's agent, in its own words.",
                "the process persona",
            ),
            field(
                "store.path",
                "Database",
                Kind::Text,
                "Its own file. Two companies sharing one is refused at startup, naming \
                 both: isolation here is a property of the filesystem.",
                "ns-<id>.sqlite",
            ),
            field(
                "engine.worker_slots",
                "Turns at once",
                Kind::Number,
                "How many of this company's conversations may be mid-turn together. What \
                 a slot overlaps is waiting, so more of them is close to free.",
                "the process default",
            ),
        ],
    },
    Section {
        title: "Their credentials",
        note: "A company signs its own tokens. Two variables mean a rotation is in \
               progress: the first is what tokens are signed with now, the second the key \
               it replaced, still accepted until everything minted under it expires.",
        fields: &[
            field(
                "auth.signing_key_envs",
                "Signing key variables",
                Kind::List,
                "One per line, current first.",
                "none",
            ),
            field(
                "auth.iat_floor",
                "Refuse tokens issued before (unix seconds)",
                Kind::Number,
                "This company's way of revoking everything minted up to a breach, without \
                 a revocation list. 0 means no floor.",
                "0",
            ),
        ],
    },
    Section {
        title: "WhatsApp",
        note: "Fill this in only for a company reachable on WhatsApp. The endpoint they \
               all arrive on is the process's; the account is theirs.",
        fields: &[
            field(
                "whatsapp.phone_number_id",
                "Business phone number id",
                Kind::Text,
                "Meta's id for the business number. The only thing in a delivery that \
                 says which company it is for — and it is inside the signed payload.",
                "unset",
            ),
            field(
                "whatsapp.access_token_env",
                "Access token variable",
                Kind::EnvName,
                "The variable holding the token replies are sent with.",
                "unset",
            ),
            field(
                "whatsapp.app_secret_env",
                "App secret variable",
                Kind::EnvName,
                "The variable holding the secret Meta signs deliveries with.",
                "unset",
            ),
            field(
                "whatsapp.session_salt_env",
                "Session salt variable",
                Kind::EnvName,
                "The variable holding the salt customer ids are hashed under, so a phone \
                 number never becomes a session id. Per company, so the same person at \
                 two companies is two unrelated subjects.",
                "unset",
            ),
        ],
    },
];

/// The field at this path, or nothing — which is the server's allowlist as
/// well as its lookup: a path no section drew a control for is a caller
/// writing keys of its own choosing, and is refused.
pub(crate) fn field_of(sections: &[Section], path: &str) -> Option<&'static Field> {
    sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|f| f.path == path)
}

/// Every variable this configuration names, and what it is for.
///
/// Built by reading the config rather than by listing the environment: a
/// page that showed every variable on the machine would be a way to learn
/// what else is running on it.
pub(crate) fn variables_named(
    base: &AppConfig,
    companies: &[(String, AppConfig)],
) -> Vec<Reference> {
    let mut named: Vec<Reference> = Vec::new();
    let mut note = |name: Option<&str>, what: &str, path: &str, company: Option<&str>| {
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            named.push(Reference {
                name: name.to_string(),
                used_for: what.to_string(),
                company: company.map(str::to_string),
                path: path.to_string(),
            });
        }
    };

    // The provider key, per role, as each role actually resolves — a role
    // pointed at another provider has another key, and the page should say
    // so rather than showing the one from `[llm]`.
    match base.llm.roles() {
        Ok(targets) => {
            for target in targets {
                if !target.local {
                    note(
                        Some(&target.api_key_env),
                        &format!("the {}'s provider key", target.role.as_str()),
                        key_path(&base.llm, target.role),
                        None,
                    );
                }
            }
        }
        // A config too broken to resolve roles still names a key.
        Err(_) => note(
            base.llm.api_key_env.as_deref(),
            "the provider key",
            "llm.api_key_env",
            None,
        ),
    }
    if base.serve.auth == crate::config::AuthMode::Shared {
        note(
            Some(&base.serve.token_env),
            "the shared socket token",
            "serve.token_env",
            None,
        );
    }
    note(
        Some(&base.http.whatsapp_verify_token_env),
        "the WhatsApp verification token",
        "http.whatsapp_verify_token_env",
        None,
    );
    if let Some(pointer) = &base.pointer {
        note(
            Some(&pointer.token_env),
            "the pointer agent's token",
            "pointer.token_env",
            None,
        );
    }

    for (id, company) in companies {
        for (index, env) in company.auth.signing_key_envs.iter().enumerate() {
            let which = if index == 0 { "current" } else { "previous" };
            note(
                Some(env),
                &format!("{id}'s {which} signing key"),
                "auth.signing_key_envs",
                Some(id),
            );
        }
        if let Some(whatsapp) = &company.whatsapp {
            note(
                Some(&whatsapp.access_token_env),
                &format!("{id}'s WhatsApp access token"),
                "whatsapp.access_token_env",
                Some(id),
            );
            note(
                Some(&whatsapp.app_secret_env),
                &format!("{id}'s WhatsApp app secret"),
                "whatsapp.app_secret_env",
                Some(id),
            );
            note(
                Some(&whatsapp.session_salt_env),
                &format!("{id}'s WhatsApp session salt"),
                "whatsapp.session_salt_env",
                Some(id),
            );
        }
        for (name, used_for, path) in company_provider_keys(id, &company.llm) {
            note(Some(&name), &used_for, &path, Some(id));
        }
    }
    named
}

/// Which key a company's own `[llm]` reaches for, and nothing more.
///
/// Only what the overlay actually says. A company with no `[llm]` of its own
/// runs on the process's provider and the process's key, and reporting that
/// as *its* key would hang a phantom reference on every company's row — and
/// make a key look shared the moment a second company existed.
fn company_provider_keys(
    id: &str,
    llm: &crate::config::LlmConfig,
) -> Vec<(String, String, String)> {
    if !picks_a_provider(llm) {
        return Vec::new();
    }
    let mut keys: Vec<(String, String, String)> = Vec::new();
    let mut note = |name: String, path: &str| {
        if !name.trim().is_empty() && !keys.iter().any(|(known, _, _)| known == &name) {
            keys.push((name, format!("{id}'s provider key"), path.to_string()));
        }
    };
    match llm.roles() {
        Ok(targets) => {
            for target in targets {
                if !target.local {
                    note(target.api_key_env.clone(), key_path(llm, target.role));
                }
            }
        }
        // Named a provider but no model the resolver can place. The
        // provider still decides the key, and that is the half of it worth
        // showing — the other half is the company's missing model, which the
        // company list reports on its own.
        Err(_) => {
            if let Some(env) = llm.api_key_env.clone() {
                note(env, "llm.api_key_env");
            } else if let Some(preset) = chosen_preset(llm).filter(|p| !p.local) {
                note(preset.api_key_env.to_string(), "llm.provider");
            }
        }
    }
    keys
}

/// Whether this `[llm]` says anything at all about which provider — and so
/// which key variable — its calls use.
///
/// There are four ways to say it and they are easy to count as three. A
/// `provider:model` prefix reads least like a provider choice and is the one
/// that gets missed: miss it and the company's real key appears in no row,
/// can never be seen as shared, and counts as nothing missing on a company
/// that cannot make a single request.
fn picks_a_provider(llm: &crate::config::LlmConfig) -> bool {
    llm.provider.is_some()
        || llm.base_url.is_some()
        || llm.api_key_env.is_some()
        || llm.emitter.api_key_env.is_some()
        || llm.summarizer.api_key_env.is_some()
        || has_prefix(&llm.emitter.model)
        || has_prefix(&llm.summarizer.model)
}

fn has_prefix(model: &Option<String>) -> bool {
    model
        .as_deref()
        .is_some_and(|spec| nsllm::provider::split_model(spec).0.is_some())
}

fn chosen_preset(llm: &crate::config::LlmConfig) -> Option<&'static nsllm::provider::Provider> {
    llm.provider
        .as_deref()
        .and_then(nsllm::provider::find)
        .or_else(|| {
            llm.emitter
                .model
                .as_deref()
                .and_then(|spec| nsllm::provider::split_model(spec).0)
        })
}

/// The control that decided the key a role resolved to, so the page can
/// point at the thing to change rather than at a field nobody set.
///
/// The order is the resolver's own (`LlmConfig::role`): the role's override,
/// then the preset a `provider:model` prefix names, then the shared `[llm]`
/// key, then the preset. Getting it wrong would send an operator to a field
/// whose value is not the one in force.
fn key_path(llm: &crate::config::LlmConfig, role: crate::config::Role) -> &'static str {
    use crate::config::Role;
    let (section_env, model) = match role {
        Role::Emitter => (&llm.emitter.api_key_env, &llm.emitter.model),
        Role::Summarizer => (&llm.summarizer.api_key_env, &llm.summarizer.model),
    };
    if section_env.is_some() {
        return match role {
            Role::Emitter => "llm.emitter.api_key_env",
            Role::Summarizer => "llm.summarizer.api_key_env",
        };
    }
    if has_prefix(model) {
        return match role {
            Role::Emitter => "llm.emitter.model",
            Role::Summarizer => "llm.summarizer.model",
        };
    }
    if llm.api_key_env.is_some() {
        return "llm.api_key_env";
    }
    if llm.provider.is_some() || llm.base_url.is_some() {
        return "llm.provider";
    }
    "llm.api_key_env"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_listed_path_may_be_written() {
        assert!(field_of(PROCESS, "llm.provider").is_some());
        assert!(field_of(COMPANY, "whatsapp.phone_number_id").is_some());
        // Not on the page, so not writable through it — including the keys
        // a company must never set for itself.
        assert!(field_of(COMPANY, "serve.listen").is_none());
        assert!(field_of(COMPANY, "http.listen").is_none());
        assert!(field_of(PROCESS, "anything.else").is_none());
    }

    /// The process-owned keys are refused in an overlay by the loader
    /// (`tenant::PROCESS_OWNED`). The page must not offer them at all, or it
    /// would present a control whose every save is refused.
    #[test]
    fn no_company_field_is_one_the_loader_would_refuse() {
        for section in COMPANY {
            for field in section.fields {
                let head = field.path.split('.').next().expect("a path has a head");
                assert_ne!(head, "http", "{}", field.path);
                assert_ne!(head, "models", "{}", field.path);
                assert!(
                    !field.path.starts_with("serve."),
                    "{} is process-owned",
                    field.path
                );
                assert_ne!(field.path, "engine.shard_worker_slots", "process-owned");
            }
        }
    }

    #[test]
    fn a_choice_field_offers_the_values_the_config_accepts() {
        let auth = field_of(PROCESS, "serve.auth").expect("the field");
        assert_eq!(auth.choices, &["shared", "jwt"]);
    }

    #[test]
    fn the_variables_listed_are_the_ones_the_config_names() {
        let base = AppConfig::parse(
            "[llm]\nprovider = \"openrouter\"\napi_key_env = \"MY_KEY\"\n\
             [serve]\nauth = \"shared\"\ntoken_env = \"MY_SOCKET_TOKEN\"\n",
        )
        .expect("parses");
        let acme = AppConfig::parse(
            "[auth]\nsigning_key_envs = [\"ACME_NOW\", \"ACME_BEFORE\"]\n\
             [whatsapp]\nphone_number_id = \"1\"\naccess_token_env = \"ACME_WA_TOKEN\"\n\
             app_secret_env = \"ACME_WA_SECRET\"\nsession_salt_env = \"ACME_WA_SALT\"\n",
        )
        .expect("parses");
        let named = variables_named(&base, &[("acme".to_string(), acme)]);
        let names: Vec<&str> = named.iter().map(|r| r.name.as_str()).collect();

        assert!(names.contains(&"MY_KEY"), "{names:?}");
        assert!(names.contains(&"MY_SOCKET_TOKEN"), "{names:?}");
        assert!(names.contains(&"ACME_WA_SECRET"), "{names:?}");
        // The rotation is described rather than just listed twice.
        let current = named
            .iter()
            .find(|r| r.name == "ACME_NOW")
            .expect("current");
        assert!(current.used_for.contains("current"), "{current:?}");
        let previous = named
            .iter()
            .find(|r| r.name == "ACME_BEFORE")
            .expect("previous");
        assert!(previous.used_for.contains("previous"), "{previous:?}");
    }

    /// T1.1. Every entry says where it is named, and whose it is — the two
    /// things that turn a list of variables into something an operator can
    /// act on without opening a file.
    #[test]
    fn every_reference_carries_its_path_and_its_owner() {
        let base = AppConfig::parse("[llm]\nprovider = \"openrouter\"\napi_key_env = \"MY_KEY\"\n")
            .expect("parses");
        let acme = AppConfig::parse("[auth]\nsigning_key_envs = [\"ACME_NOW\"]\n").expect("parses");
        let named = variables_named(&base, &[("acme".to_string(), acme)]);

        let process = named.iter().find(|r| r.name == "MY_KEY").expect("the key");
        assert_eq!(process.path, "llm.api_key_env");
        assert_eq!(process.company, None, "the process's own");

        let theirs = named.iter().find(|r| r.name == "ACME_NOW").expect("theirs");
        assert_eq!(theirs.path, "auth.signing_key_envs");
        assert_eq!(theirs.company.as_deref(), Some("acme"));
    }

    /// A company with no `[llm]` of its own is not a company referencing
    /// the process's key: it would make that key look shared as soon as a
    /// second company existed, and sharing is the one thing this page must
    /// not cry wolf about.
    #[test]
    fn a_company_without_its_own_provider_references_no_key() {
        let base =
            AppConfig::parse("[llm]\nprovider = \"openrouter\"\napi_key_env = \"SHARED_KEY\"\n")
                .expect("parses");
        let plain = AppConfig::parse("[store]\npath = \"ns-acme.sqlite\"\n").expect("parses");
        let named = variables_named(&base, &[("acme".to_string(), plain)]);
        assert!(
            !named.iter().any(|r| r.company.is_some()),
            "a company claimed a key it never named: {named:?}"
        );
    }

    /// A `provider:model` prefix picks a provider, and a provider brings a
    /// key variable with it. Missing it would leave the company's real key
    /// in no row at all — unlistable, never shared, and counted as nothing
    /// missing on a company that cannot make a single request.
    #[test]
    fn a_company_that_picks_its_provider_in_the_model_prefix_still_names_a_key() {
        let base = AppConfig::parse("[llm]\napi_key_env = \"HOUSE_KEY\"\n").expect("parses");
        let acme = AppConfig::parse("[llm.emitter]\nmodel = \"mistral:mistral-small-latest\"\n")
            .expect("parses");
        let named = variables_named(&base, &[("acme".to_string(), acme)]);
        let theirs = named
            .iter()
            .find(|r| r.company.as_deref() == Some("acme"))
            .expect("acme names a key through the prefix");
        assert_eq!(theirs.name, "MISTRAL_API_KEY");
        // And the row points at the control that actually chose it.
        assert_eq!(theirs.path, "llm.emitter.model");
    }

    /// The prefix beats `[llm] api_key_env` in the resolver, so it has to
    /// beat it here too — otherwise the page names a variable whose value
    /// is not the one the call is made with.
    #[test]
    fn a_prefix_outranks_the_shared_key_the_way_the_resolver_does() {
        let base = AppConfig::parse(
            "[llm]\napi_key_env = \"HOUSE_KEY\"\n[llm.emitter]\nmodel = \"mistral:m\"\n",
        )
        .expect("parses");
        let named = variables_named(&base, &[]);
        let emitter = named
            .iter()
            .find(|r| r.used_for.contains("emitter"))
            .expect("the emitter's key");
        assert_eq!(emitter.name, "MISTRAL_API_KEY");
        assert_eq!(emitter.path, "llm.emitter.model");
    }

    /// T2.2's half of the vault: a company that names its own key is a
    /// reference like any other, and the row can say whose it is.
    #[test]
    fn a_company_with_its_own_key_is_a_reference_of_its_own() {
        let base = AppConfig::parse("[llm]\napi_key_env = \"SHARED_KEY\"\n").expect("parses");
        let acme =
            AppConfig::parse("[llm]\nprovider = \"groq\"\napi_key_env = \"ACME_PROVIDER_KEY\"\n")
                .expect("parses");
        let named = variables_named(&base, &[("acme".to_string(), acme)]);
        let theirs = named
            .iter()
            .find(|r| r.name == "ACME_PROVIDER_KEY")
            .expect("the company's own key");
        assert_eq!(theirs.company.as_deref(), Some("acme"));
        assert_eq!(theirs.path, "llm.api_key_env");
    }

    /// A local server needs no key, so naming one on the page would be a
    /// box nobody has to fill in and everybody would try to.
    #[test]
    fn a_local_provider_names_no_key() {
        let base = AppConfig::parse("[llm]\nprovider = \"ollama\"\n").expect("parses");
        let named = variables_named(&base, &[]);
        let names: Vec<&str> = named.iter().map(|r| r.name.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.contains("API_KEY")),
            "a local provider asked for a key: {names:?}"
        );
    }
}
