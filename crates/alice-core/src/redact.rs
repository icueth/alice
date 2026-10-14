//! Secret detection and redaction (spec §5.3). Runs before anything is written or sent.
//! Matches become `[REDACTED:<detector>]`; `block` mode refuses the text instead, naming the
//! detector (never the secret). Already-redacted text is left as it is.
//!
//! ```
//! use alice_core::redact::Redactor;
//!
//! let r = Redactor::builtin();
//! let out = r.apply("db postgres://admin:s3cr3tPass@db.internal:5432/app");
//! assert_eq!(out.found, ["url_credentials"]);
//! assert!(!out.text.contains("s3cr3tPass"));
//! ```

use crate::error::{AliceError, Result};
use crate::settings::{PrivacySettings, RedactMode};
use regex::Regex;

struct Detector {
    name: String,
    re: Regex,
    /// Capture group to replace (0 = the whole match).
    group: usize,
    /// Extra test on the replaced text (false = not a secret).
    check: fn(&str) -> bool,
}

/// A compiled set of detectors. Build it once and share it; compiling costs milliseconds.
pub struct Redactor {
    enabled: bool,
    mode: RedactMode,
    detectors: Vec<Detector>,
    allow: Vec<Regex>,
}

impl std::fmt::Debug for Redactor {
    /// Detector names only: a custom pattern's regular expression is the user's business.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("enabled", &self.enabled)
            .field("mode", &self.mode)
            .field(
                "detectors",
                &self.detectors.iter().map(|d| &d.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// What a `block`-mode refusal says after the field's name.
const REFUSAL: &str = "contains what looks like a secret";

/// Whether `e` is a `block`-mode refusal from [`Redactor::check`]: the write held a secret and
/// was not made. A caller that keeps refused requests (the spool) must not keep this one's text.
pub fn is_refusal(e: &AliceError) -> bool {
    matches!(e, AliceError::Invalid(m) if m.contains(REFUSAL))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    /// Detector names that fired, in order of first appearance.
    pub found: Vec<String>,
}

/// Shannon entropy in bits per character.
pub fn entropy(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    let n = s.chars().count() as f64;
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    counts
        .values()
        .map(|&k| {
            let p = k as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn any(_: &str) -> bool {
    true
}

/// A value after `password =` etc. that is a secret, not code or prose: no call, index,
/// template or dotted path; not an ENV_NAME; and either letters with digits or long and varied.
fn secret_value(v: &str) -> bool {
    if v.starts_with("[REDACTED") || v.contains(['(', ')', '[', ']', '{', '}', '<', '>', '$']) {
        return false;
    }
    let dotted = v.contains('.')
        && v.split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_'));
    let env_name = v
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if dotted || env_name {
        return false;
    }
    let letters = v.chars().any(|c| c.is_alphabetic());
    let digits = v.chars().any(|c| c.is_ascii_digit());
    (letters && digits) || (v.chars().count() >= 16 && entropy(v) >= 3.0)
}

fn high_entropy(v: &str) -> bool {
    !v.starts_with("[REDACTED") && entropy(v) >= 3.5
}

fn builtin_detectors() -> Vec<Detector> {
    let d = |name: &str, re: &str, group: usize, check: fn(&str) -> bool| Detector {
        name: name.into(),
        re: Regex::new(re).expect("built-in pattern"),
        group,
        check,
    };
    vec![
        d(
            "pem_private_key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
            0,
            any,
        ),
        d(
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
            any,
        ),
        d("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", 0, any),
        d(
            "aws_secret_key",
            r#"(?i)\baws_?secret_?(?:access_?)?key\b\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})"#,
            1,
            any,
        ),
        d(
            "gcp_key_id",
            r#""private_key_id"\s*:\s*"([0-9a-f]{40})""#,
            1,
            any,
        ),
        d(
            "github_token",
            r"\b(?:(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b",
            0,
            any,
        ),
        d("gitlab_token", r"\bglpat-[A-Za-z0-9_\-]{20,}", 0, any),
        d("slack_token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}", 0, any),
        d("stripe_key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{16,}", 0, any),
        d("anthropic_key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}", 0, any),
        d(
            "openai_key",
            r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_\-]{20,}",
            0,
            any,
        ),
        d(
            "url_credentials",
            r"\b[A-Za-z][A-Za-z0-9+.\-]*://[^\s/:@\[\]]+:([^\s/@\[\]]+)@",
            1,
            any,
        ),
        d(
            "password_assignment",
            r#"(?i)(?:\b(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|auth[_-]?token|client[_-]?secret)\b|รหัสผ่าน)\s*[:=]\s*["']?([^\s"',;]{6,})"#,
            1,
            secret_value,
        ),
        d(
            "high_entropy",
            r#"(?i)\b[A-Za-z_][A-Za-z0-9_]*(?:key|secret|token|pass|pwd|auth|credential)[A-Za-z0-9_]*\s*[:=]\s*["']?([A-Za-z0-9+/=_\-]{32,})"#,
            1,
            high_entropy,
        ),
    ]
}

impl Redactor {
    /// The built-in detectors with the default settings (redact mode, no exceptions).
    pub fn builtin() -> Self {
        Self::new(&PrivacySettings::default()).expect("built-in detectors compile")
    }

    /// The built-in detectors with `p`'s switch and mode, ignoring its custom patterns and
    /// allowlist. What a caller uses when [`new`](Self::new) refuses `p`: never "no redaction",
    /// and an allowlist that cannot be read is an allowlist that keeps nothing.
    pub fn fallback(p: &PrivacySettings) -> Self {
        Self {
            enabled: p.redact,
            mode: p.redact_mode,
            detectors: builtin_detectors(),
            allow: Vec::new(),
        }
    }

    /// The detectors for `p`: the built-in ones, then its custom patterns. An error names the
    /// setting that is wrong, never the pattern.
    pub fn new(p: &PrivacySettings) -> Result<Self> {
        let mut detectors = builtin_detectors();
        for c in &p.custom_patterns {
            let re = Regex::new(&c.regex).map_err(|_| {
                AliceError::invalid(format!(
                    "privacy.custom_patterns \"{}\" is not a valid regular expression; fix it in config.toml or .alice.toml",
                    c.name
                ))
            })?;
            if re.is_match("") {
                return Err(AliceError::invalid(format!(
                    "privacy.custom_patterns \"{}\" matches empty text; make it match something",
                    c.name
                )));
            }
            detectors.push(Detector {
                name: format!("custom:{}", c.name),
                re,
                group: 0,
                check: any,
            });
        }
        let allow = p
            .allowlist
            .iter()
            .map(|a| Regex::new(&format!("^(?:{a})$")))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| {
                AliceError::invalid(
                    "privacy.allowlist holds an invalid regular expression; fix it in config.toml",
                )
            })?;
        Ok(Self {
            enabled: p.redact,
            mode: p.redact_mode,
            detectors,
            allow,
        })
    }

    /// Whether this redactor refuses text with a secret instead of redacting it.
    pub fn blocks(&self) -> bool {
        self.enabled && self.mode == RedactMode::Block
    }

    /// `text` with every secret replaced, and the detectors that found one.
    pub fn apply(&self, text: &str) -> Redaction {
        let mut out = text.to_string();
        let mut found: Vec<String> = Vec::new();
        if !self.enabled {
            return Redaction { text: out, found };
        }
        for d in &self.detectors {
            let tag = format!("[REDACTED:{}]", d.name);
            let mut result = String::with_capacity(out.len());
            let mut last = 0;
            let mut hit = false;
            for caps in d.re.captures_iter(&out) {
                let Some(m) = caps.get(d.group) else { continue };
                let value = m.as_str();
                if value.is_empty()
                    || !(d.check)(value)
                    || self.allow.iter().any(|a| a.is_match(value))
                {
                    continue;
                }
                result.push_str(&out[last..m.start()]);
                result.push_str(&tag);
                last = m.end();
                hit = true;
            }
            if hit {
                result.push_str(&out[last..]);
                out = result;
                if !found.contains(&d.name) {
                    found.push(d.name.clone());
                }
            }
        }
        Redaction { text: out, found }
    }

    /// Every string inside `value` redacted (keys are left alone); the detectors that fired.
    /// For snapshots recorded in the history, which is never refused, only cleaned.
    pub fn apply_json(&self, value: &mut serde_json::Value) -> Vec<String> {
        let mut found = Vec::new();
        self.walk(value, &mut found);
        found
    }

    fn walk(&self, value: &mut serde_json::Value, found: &mut Vec<String>) {
        use serde_json::Value;
        match value {
            Value::String(s) => {
                let r = self.apply(s);
                if !r.found.is_empty() {
                    *s = r.text;
                    for f in r.found {
                        if !found.contains(&f) {
                            found.push(f);
                        }
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.walk(v, found)),
            Value::Object(map) => map.values_mut().for_each(|v| self.walk(v, found)),
            _ => {}
        }
    }

    /// The text to store for `field`: redacted, or refused in `block` mode.
    ///
    /// ```
    /// use alice_core::redact::Redactor;
    /// use alice_core::settings::{PrivacySettings, RedactMode};
    ///
    /// let block = Redactor::new(&PrivacySettings {
    ///     redact_mode: RedactMode::Block,
    ///     ..Default::default()
    /// })
    /// .unwrap();
    /// let err = block.check("content", "password = hunter2").unwrap_err().to_string();
    /// assert!(err.contains("password_assignment") && !err.contains("hunter2"));
    /// ```
    pub fn check(&self, field: &str, text: &str) -> Result<String> {
        let r = self.apply(text);
        if self.mode == RedactMode::Block && !r.found.is_empty() {
            return Err(AliceError::invalid(format!(
                "the {field} {REFUSAL} ({}); remove it and send again, or add an exception to privacy.allowlist",
                r.found.join(", ")
            )));
        }
        Ok(r.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{CustomPattern, PrivacySettings, RedactMode};

    /// (input, detector expected to fire, text that must be gone)
    const SECRETS: &[(&str, &str, &str)] = &[
        (
            concat!("key AKIA", "IOSFODNN7EXAMPLE in env"),
            "aws_access_key",
            concat!("AKIA", "IOSFODNN7EXAMPLE"),
        ),
        (
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "aws_secret_key",
            "wJalrXUtnFEMI/K7MDENG",
        ),
        (
            "{\"private_key_id\": \"1234567890abcdef1234567890abcdef12345678\"}",
            "gcp_key_id",
            "1234567890abcdef1234567890abcdef12345678",
        ),
        (
            concat!("push with gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
            "github_token",
            concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
        ),
        (
            concat!(
                "github_",
                "pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz"
            ),
            "github_token",
            concat!("github_", "pat_11ABCDEFG"),
        ),
        (
            concat!("gl", "pat-abcdefghij0123456789xyz"),
            "gitlab_token",
            concat!("gl", "pat-abcdefghij0123456789"),
        ),
        (
            concat!("slack xo", "xb-123456789012-abcdefghijkl"),
            "slack_token",
            concat!("xo", "xb-123456789012"),
        ),
        (
            concat!("stripe sk_", "live_51HabcdefghijklmnopqrSTU"),
            "stripe_key",
            concat!("sk_", "live_51Habcdef"),
        ),
        (
            concat!(
                "ANTHROPIC sk-",
                "ant-api03-abcdefghijklmnopqrstuvwxyz012345"
            ),
            "anthropic_key",
            concat!("sk-", "ant-api03-abcdef"),
        ),
        (
            concat!("openai sk-", "proj-abcdefghijklmnopqrstuvwxyz0123"),
            "openai_key",
            concat!("sk-", "proj-abcdefghijkl"),
        ),
        (
            concat!(
                "jwt ey",
                "JhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
            ),
            "jwt",
            concat!("ey", "JhbGciOiJIUzI1NiJ9"),
        ),
        (
            concat!(
                "-----BEGIN RSA PRIVATE ",
                "KEY-----\nMIIEpAIBAAKCAQEA1234\n-----END RSA PRIVATE KEY-----"
            ),
            "pem_private_key",
            "MIIEpAIBAAKCAQEA1234",
        ),
        (
            "db postgres://admin:s3cr3tPass@db.internal:5432/app",
            "url_credentials",
            "s3cr3tPass",
        ),
        ("password = hunter2", "password_assignment", "hunter2"),
        (
            "API_KEY: \"correcthorsebatterystaple\"",
            "password_assignment",
            "correcthorsebatterystaple",
        ),
        (
            "รหัสผ่าน: abc12345 สำหรับเครื่องทดสอบ",
            "password_assignment",
            "abc12345",
        ),
        (
            "STRIPE_SIGNING_KEY=whsec_9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a",
            "high_entropy",
            "whsec_9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a",
        ),
    ];

    /// Text that must come out unchanged (code and prose that only look like secrets).
    const SAFE: &[&str] = &[
        "let token = parse_token(input);",
        "password: required",
        "token: String,",
        "api_key = os.environ[\"API_KEY\"]",
        "secret = config.secret_value",
        "set the TOKEN env var to your-token-here",
        "the sk-learn library and sketching",
        "http://example.com/path@section",
        "ใช้ WAL mode เพื่อให้ reader ไม่บล็อก writer",
        "already [REDACTED:github_token] here",
    ];

    #[test]
    fn every_builtin_detector_fires_and_removes_the_secret() {
        let r = Redactor::builtin();
        for (input, detector, secret) in SECRETS {
            let out = r.apply(input);
            assert!(
                out.found.contains(&detector.to_string()),
                "{input:?}: found {:?}, text {:?}",
                out.found,
                out.text
            );
            assert!(!out.text.contains(secret), "{input:?} → {:?}", out.text);
            assert!(
                out.text.contains(&format!("[REDACTED:{detector}]")),
                "{:?}",
                out.text
            );
        }
    }

    #[test]
    fn code_and_prose_are_left_alone_and_redaction_is_idempotent() {
        let r = Redactor::builtin();
        for s in SAFE {
            let out = r.apply(s);
            assert_eq!(out.text, *s, "{s:?} changed ({:?})", out.found);
        }
        for (input, _, _) in SECRETS {
            let once = r.apply(input).text;
            assert_eq!(r.apply(&once).text, once, "{input:?}");
        }
    }

    #[test]
    fn block_mode_names_the_detector_and_off_keeps_text() {
        let block = Redactor::new(&PrivacySettings {
            redact_mode: RedactMode::Block,
            ..Default::default()
        })
        .unwrap();
        let err = block
            .check(
                "content",
                concat!("deploy with gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("github_token") && !err.contains("ghp_"),
            "{err}"
        );
        assert_eq!(
            block.check("content", "nothing secret").unwrap(),
            "nothing secret"
        );
        let off = Redactor::new(&PrivacySettings {
            redact: false,
            ..Default::default()
        })
        .unwrap();
        assert!(
            off.check("content", "password = hunter2")
                .unwrap()
                .contains("hunter2")
        );
    }

    #[test]
    fn custom_patterns_and_the_allowlist() {
        let p = PrivacySettings {
            custom_patterns: vec![CustomPattern {
                name: "ticket".into(),
                regex: r"TCK-[0-9]{4,}".into(),
            }],
            allowlist: vec![concat!("AKIA", "IOSFODNN7EXAMPLE").into()],
            ..Default::default()
        };
        let r = Redactor::new(&p).unwrap();
        let out = r.apply(concat!("see TCK-12345 and AKIA", "IOSFODNN7EXAMPLE"));
        assert_eq!(
            out.text,
            concat!("see [REDACTED:custom:ticket] and AKIA", "IOSFODNN7EXAMPLE")
        );
        assert_eq!(out.found, vec!["custom:ticket".to_string()]);
    }

    #[test]
    fn a_bad_pattern_is_refused_without_echoing_it_and_the_fallback_still_redacts() {
        let secret_looking = "(unclosed-TCK";
        let p = PrivacySettings {
            custom_patterns: vec![
                CustomPattern {
                    name: "broken".into(),
                    regex: secret_looking.into(),
                },
                CustomPattern {
                    name: "empty".into(),
                    regex: "x*".into(),
                },
            ],
            redact_mode: RedactMode::Block,
            ..Default::default()
        };
        let e = Redactor::new(&p).unwrap_err().to_string();
        assert!(e.contains("broken") && !e.contains(secret_looking), "{e}");
        let only_empty = PrivacySettings {
            custom_patterns: vec![p.custom_patterns[1].clone()],
            ..Default::default()
        };
        assert!(
            Redactor::new(&only_empty)
                .unwrap_err()
                .to_string()
                .contains("empty")
        );
        let f = Redactor::fallback(&p);
        assert!(f.blocks());
        assert!(f.check("content", "password = hunter2").is_err());
        let off = Redactor::fallback(&PrivacySettings {
            redact: false,
            ..Default::default()
        });
        assert_eq!(off.apply("password = hunter2").found.len(), 0);
        let debug = format!("{:?}", Redactor::new(&PrivacySettings::default()).unwrap());
        assert!(debug.contains("pem_private_key") && !debug.contains("BEGIN"));
    }

    #[test]
    fn json_strings_are_redacted_wherever_they_sit() {
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let mut v = serde_json::json!({
            "title": format!("t {token}"),
            "topics": [token, "plain"],
            "nested": {"n": 3, "body": "password = hunter2"},
        });
        let found = Redactor::builtin().apply_json(&mut v);
        let text = v.to_string();
        assert!(!text.contains(token) && !text.contains("hunter2"), "{text}");
        let mut found = found;
        found.sort();
        assert_eq!(found, ["github_token", "password_assignment"]);
        assert_eq!(v["nested"]["n"], 3);
        assert_eq!(v["topics"][1], "plain");
    }
}
