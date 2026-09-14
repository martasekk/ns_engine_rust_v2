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
               here — the config names a variable, and Credentials is where that variable \
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
                "Emitter model",
                Kind::Text,
                "The model that proposes actions. This is the one that decides how good \
                 the agent is.",
                "the provider's default",
            ),
            field(
                "llm.replier.model",
                "Replier model",
                Kind::Text,
                "The model that writes the reply from the trace of what actually \
                 happened. It never decides what was done.",
                "the emitter's",
            ),
            choice(
                "llm.capability",
                "Model class",
                &["small", "strong"],
                "`strong` stands down the scaffolding that exists to compensate for a \
                 weak emitter. It never adds any.",
                "small",
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
) -> Vec<(String, String)> {
    let mut named: Vec<(String, String)> = Vec::new();
    let mut note = |name: Option<&str>, what: &str| {
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            named.push((name.to_string(), what.to_string()));
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
                    );
                }
            }
        }
        // A config too broken to resolve roles still names a key.
        Err(_) => note(base.llm.api_key_env.as_deref(), "the provider key"),
    }
    if base.serve.auth == crate::config::AuthMode::Shared {
        note(Some(&base.serve.token_env), "the shared socket token");
    }
    note(
        Some(&base.http.whatsapp_verify_token_env),
        "the WhatsApp verification token",
    );
    if let Some(pointer) = &base.pointer {
        note(Some(&pointer.token_env), "the pointer agent's token");
    }

    for (id, company) in companies {
        for (index, env) in company.auth.signing_key_envs.iter().enumerate() {
            let which = if index == 0 { "current" } else { "previous" };
            note(Some(env), &format!("{id}'s {which} signing key"));
        }
        if let Some(whatsapp) = &company.whatsapp {
            note(
                Some(&whatsapp.access_token_env),
                &format!("{id}'s WhatsApp access token"),
            );
            note(
                Some(&whatsapp.app_secret_env),
                &format!("{id}'s WhatsApp app secret"),
            );
            note(
                Some(&whatsapp.session_salt_env),
                &format!("{id}'s WhatsApp session salt"),
            );
        }
    }
    named
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
        let capability = field_of(PROCESS, "llm.capability").expect("the field");
        assert_eq!(capability.choices, &["small", "strong"]);
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
        let names: Vec<&str> = named.iter().map(|(n, _)| n.as_str()).collect();

        assert!(names.contains(&"MY_KEY"), "{names:?}");
        assert!(names.contains(&"MY_SOCKET_TOKEN"), "{names:?}");
        assert!(names.contains(&"ACME_WA_SECRET"), "{names:?}");
        // The rotation is described rather than just listed twice.
        let current = named
            .iter()
            .find(|(n, _)| n == "ACME_NOW")
            .expect("current");
        assert!(current.1.contains("current"), "{:?}", current.1);
        let previous = named
            .iter()
            .find(|(n, _)| n == "ACME_BEFORE")
            .expect("previous");
        assert!(previous.1.contains("previous"), "{:?}", previous.1);
    }

    /// A local server needs no key, so naming one on the page would be a
    /// box nobody has to fill in and everybody would try to.
    #[test]
    fn a_local_provider_names_no_key() {
        let base = AppConfig::parse("[llm]\nprovider = \"ollama\"\n").expect("parses");
        let named = variables_named(&base, &[]);
        let names: Vec<&str> = named.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.contains("API_KEY")),
            "a local provider asked for a key: {names:?}"
        );
    }
}
