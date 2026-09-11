//! Learned rules (spec M5 §3): deterministic input repairs the engine applies
//! before legality/validation, plus guidance notes for the emitter prompt.
//! Application is pure; loading/saving lives in ns-evolution.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Op {
    Trim,
    StripPunct,
    Lowercase,
    StripPrefix(String),
}

impl Op {
    pub fn apply(&self, s: &str) -> String {
        match self {
            Op::Trim => s.trim().to_string(),
            Op::StripPunct => s
                .trim_matches(|c: char| c.is_ascii_punctuation())
                .to_string(),
            Op::Lowercase => s.to_lowercase(),
            Op::StripPrefix(p) => s.strip_prefix(p.as_str()).unwrap_or(s).to_string(),
        }
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Trim => write!(f, "trim"),
            Op::StripPunct => write!(f, "strip_punct"),
            Op::Lowercase => write!(f, "lowercase"),
            Op::StripPrefix(p) => write!(f, "strip_prefix:{p}"),
        }
    }
}

impl std::str::FromStr for Op {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trim" => Ok(Op::Trim),
            "strip_punct" => Ok(Op::StripPunct),
            "lowercase" => Ok(Op::Lowercase),
            _ => match s.strip_prefix("strip_prefix:") {
                Some(p) if !p.is_empty() => Ok(Op::StripPrefix(p.to_string())),
                _ => Err(format!("unknown op {s:?}")),
            },
        }
    }
}

impl TryFrom<String> for Op {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Op> for String {
    fn from(op: Op) -> String {
        op.to_string()
    }
}

pub fn apply_ops(ops: &[Op], s: &str) -> String {
    ops.iter().fold(s.to_string(), |acc, op| op.apply(&acc))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizeArg {
    pub action: String,
    pub arg: String,
    pub ops: Vec<Op>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AliasAction {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// "global" or "action:<name>"
    pub scope: String,
    pub text: String,
    #[serde(default)]
    pub lift: f64,
    #[serde(default)]
    pub hash: String,
}

impl Note {
    pub fn new(scope: &str, text: &str, lift: f64) -> Note {
        Note {
            scope: scope.into(),
            text: text.into(),
            lift,
            hash: Note::hash_of(scope, text),
        }
    }
    pub fn hash_of(scope: &str, text: &str) -> String {
        let mut h = Sha256::new();
        h.update(scope.as_bytes());
        h.update(b"\n");
        h.update(text.as_bytes());
        format!("sha256:{:x}", h.finalize())
    }
    pub fn applies_to(&self, legal: &[String]) -> bool {
        match self.scope.strip_prefix("action:") {
            None => self.scope == "global",
            Some(name) => legal.iter().any(|l| l == name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearnedRules {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub normalize_arg: Vec<NormalizeArg>,
    #[serde(default)]
    pub alias_action: Vec<AliasAction>,
    #[serde(default, rename = "note")]
    pub notes: Vec<Note>,
}

fn default_version() -> u32 {
    1
}

impl Default for LearnedRules {
    fn default() -> Self {
        Self {
            version: 1,
            normalize_arg: vec![],
            alias_action: vec![],
            notes: vec![],
        }
    }
}

impl LearnedRules {
    /// First alias whose `from` matches.
    pub fn alias(&self, action: &str) -> Option<&str> {
        self.alias_action
            .iter()
            .find(|a| a.from == action)
            .map(|a| a.to.as_str())
    }

    /// Rewrite string args in place per the rules for `action`; other value
    /// types and unknown actions are left untouched.
    pub fn normalize(&self, action: &str, args: &mut serde_json::Value) {
        let Some(obj) = args.as_object_mut() else {
            return;
        };
        for rule in self.normalize_arg.iter().filter(|r| r.action == action) {
            if let Some(serde_json::Value::String(s)) = obj.get_mut(&rule.arg) {
                *s = apply_ops(&rule.ops, s);
            }
        }
    }

    /// Global notes plus `action:<name>` notes for names in `legal`, in file
    /// order, each with its hash.
    ///
    /// The hash is what makes a rendered note identifiable afterwards: the
    /// text lives in `learned.toml`, which the evolution pass rewrites, so
    /// only the hash survives into the manifest (M9 T0.3).
    pub fn guidance_notes_for(&self, legal: &[String]) -> Vec<(String, String)> {
        self.notes
            .iter()
            .filter(|n| n.applies_to(legal))
            .map(|n| (n.hash.clone(), n.text.clone()))
            .collect()
    }

    /// Notes scoped `reply` (M6 §8.5) with their hashes.
    pub fn guidance_notes_for_reply(&self) -> Vec<(String, String)> {
        self.notes
            .iter()
            .filter(|n| n.scope == "reply")
            .map(|n| (n.hash.clone(), n.text.clone()))
            .collect()
    }

    /// Global notes plus `action:<name>` notes for names in `legal`, in file order.
    pub fn guidance_for(&self, legal: &[String]) -> Vec<String> {
        self.guidance_notes_for(legal)
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    /// Notes scoped `reply` (M6 §8.5): rendered to the reply model only.
    pub fn guidance_for_reply(&self) -> Vec<String> {
        self.guidance_notes_for_reply()
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ops_apply_and_round_trip_as_strings() {
        assert_eq!(Op::Trim.apply("  x "), "x");
        assert_eq!(Op::StripPunct.apply("\"Brno.\""), "Brno");
        assert_eq!(Op::Lowercase.apply("Celsius"), "celsius");
        assert_eq!(Op::StripPrefix("city:".into()).apply("city:Brno"), "Brno");
        assert_eq!(Op::StripPrefix("city:".into()).apply("Brno"), "Brno");
        for op in [
            Op::Trim,
            Op::StripPunct,
            Op::Lowercase,
            Op::StripPrefix("a:b".into()),
        ] {
            assert_eq!(op.to_string().parse::<Op>().unwrap(), op);
        }
        assert!("explode".parse::<Op>().is_err());
        assert_eq!(
            apply_ops(&[Op::Trim, Op::StripPunct, Op::Lowercase], " \"Celsius\". "),
            "celsius"
        );
    }

    #[test]
    fn toml_round_trip_with_ops_as_strings() {
        let text = r#"
            version = 1
            [[normalize_arg]]
            action = "get_weather"
            arg = "unit"
            ops = ["trim", "lowercase", "strip_prefix:unit="]
            [[alias_action]]
            from = "getTime"
            to = "get_time"
            [[note]]
            scope = "global"
            text = "Prefer a tool over guessing."
            lift = 0.5
            hash = "sha256:abc"
        "#;
        let rules: LearnedRules = toml::from_str(text).unwrap();
        assert_eq!(
            rules.normalize_arg[0].ops,
            vec![Op::Trim, Op::Lowercase, Op::StripPrefix("unit=".into())]
        );
        assert_eq!(rules.alias("getTime"), Some("get_time"));
        let back = toml::to_string(&rules).unwrap();
        assert!(back.contains("\"strip_prefix:unit=\""), "{back}");
        assert_eq!(toml::from_str::<LearnedRules>(&back).unwrap(), rules);
        assert_eq!(
            toml::from_str::<LearnedRules>("").unwrap(),
            LearnedRules::default()
        );
    }

    #[test]
    fn normalize_rewrites_only_matching_string_args() {
        let rules = LearnedRules {
            normalize_arg: vec![NormalizeArg {
                action: "get_weather".into(),
                arg: "unit".into(),
                ops: vec![Op::Trim, Op::Lowercase],
            }],
            ..Default::default()
        };
        let mut args = json!({"unit": " Celsius", "city": " Brno", "n": 3});
        rules.normalize("get_weather", &mut args);
        assert_eq!(args, json!({"unit": "celsius", "city": " Brno", "n": 3}));
        let mut other = json!({"unit": " Celsius"});
        rules.normalize("other_action", &mut other);
        assert_eq!(other, json!({"unit": " Celsius"}));
        let mut num = json!({"unit": 7});
        rules.normalize("get_weather", &mut num);
        assert_eq!(num, json!({"unit": 7}));
    }

    #[test]
    fn guidance_is_global_plus_legal_action_scoped() {
        let rules = LearnedRules {
            notes: vec![
                Note::new("global", "G", 0.0),
                Note::new("action:echo", "E", 0.0),
                Note::new("action:wipe", "W", 0.0),
            ],
            ..Default::default()
        };
        assert_eq!(
            rules.guidance_for(&["echo".to_string()]),
            vec!["G".to_string(), "E".to_string()]
        );
        assert_eq!(rules.guidance_for(&[]), vec!["G".to_string()]);
    }

    #[test]
    fn reply_scoped_notes_go_to_the_replier_only() {
        let rules = LearnedRules {
            notes: vec![
                Note::new("global", "G", 0.0),
                Note::new("reply", "Answer the question first.", 0.0),
            ],
            ..Default::default()
        };
        assert_eq!(
            rules.guidance_for_reply(),
            vec!["Answer the question first.".to_string()]
        );
        assert_eq!(rules.guidance_for(&[]), vec!["G".to_string()]);
    }

    #[test]
    fn note_hash_is_stable_and_scope_sensitive() {
        let a = Note::new("global", "x", 0.0);
        assert!(a.hash.starts_with("sha256:") && a.hash.len() == 7 + 64);
        assert_eq!(a.hash, Note::hash_of("global", "x"));
        assert_ne!(
            Note::hash_of("global", "x"),
            Note::hash_of("action:echo", "x")
        );
    }
}
