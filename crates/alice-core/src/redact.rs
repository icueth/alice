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

/// Whether `e` is a `block`-mode refusal from [`Redactor::check`]: the write held a secret and
/// was not made. A caller that keeps refused requests (the spool) must not keep this one's text.
pub fn is_refusal(e: &AliceError) -> bool {
    matches!(e, AliceError::Refused { .. })
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

/// Prefixes of function names that read a value from somewhere: `token = get_token_from_cache`
/// names code that fetches a secret, it is not one.
const ACCESSOR_PREFIXES: &[&str] = &[
    "get_",
    "load_",
    "read_",
    "fetch_",
    "parse_",
    "make_",
    "build_",
    "create_",
    "new_",
    "resolve_",
    "lookup_",
    "generate_",
    "compute_",
];

/// A plain identifier of at most 32 characters that does not look like (part of) a token: not
/// 16 or more characters mixing letter cases or letters and digits, and not high-entropy.
fn plain_identifier(p: &str) -> bool {
    let starts = p
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let ident = starts && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ident || p.len() > 32 {
        return false;
    }
    let lower = p.chars().any(|c| c.is_ascii_lowercase());
    let upper = p.chars().any(|c| c.is_ascii_uppercase());
    let digit = p.chars().any(|c| c.is_ascii_digit());
    let tokenish = p.len() >= 16 && ((lower && upper) || (digit && (lower || upper)));
    !tokenish && entropy(p) < HIGH_ENTROPY
}

/// A dotted attribute access (`config.secret_value`, `process.env.X`): every segment a plain
/// identifier, and not one segment repeated three or more times (`hunter2.hunter2.hunter2`).
/// Dotted provider tokens (`SG.xxxx.yyyy`, `hvs.xxxx`) have a token-like segment and fail this.
fn attribute_access(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() > 1
        && parts.iter().all(|p| plain_identifier(p))
        && !(parts.len() >= 3 && parts.iter().all(|p| *p == parts[0]))
}

/// A value that is code and not a secret: a call or an index, a dotted attribute access, or a
/// call to an accessor by name. A plain `snake_case` word is not evidence of code: passphrases
/// look like that, so redaction errs toward redacting it.
fn code_like(v: &str) -> bool {
    v.contains(['(', '['])
        || attribute_access(v)
        || ACCESSOR_PREFIXES.iter().any(|a| v.starts_with(a))
}

/// A path segment that reads as a word: path characters only, and not base64-shaped. CamelCase
/// (`MyCoolProject`) is a word; a segment with digits among several case changes
/// (`4AAQSkZJRgAB`) is not, and neither is one with very many case changes.
fn path_word(seg: &str) -> bool {
    let ok = seg
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let chars: Vec<char> = seg.chars().collect();
    let rises = chars
        .windows(2)
        .filter(|w| w[0].is_lowercase() && w[1].is_uppercase())
        .count();
    let digits = chars.iter().any(|c| c.is_ascii_digit());
    ok && if digits { rises < 2 } else { rises <= 4 }
}

/// A file path rather than a secret: it starts with `/`, `~/`, `./` or `../`, has at least two
/// segments that read as words, and is not random-looking. Base64 such as `/9j/4AAQ...` has the
/// same outline and must still be redacted. (The entropy bound is higher than the detectors' own,
/// because real paths such as `/Users/icue/Code/AI/project-alice` reach 4 bits per character;
/// the word test does the separating.)
fn path_like(v: &str) -> bool {
    let rest = ["~/", "./", "../", "/"]
        .iter()
        .find_map(|p| v.strip_prefix(p));
    let Some(rest) = rest else { return false };
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    segments.len() >= 2 && segments.iter().all(|s| path_word(s)) && entropy(v) < PATH_ENTROPY
}

/// Bits per character from which a path-shaped value counts as random.
const PATH_ENTROPY: f64 = 4.5;

/// Bits per character from which a long token-shaped value counts as random.
const HIGH_ENTROPY: f64 = 3.5;

/// A value after `password =` etc. that is a secret, not code or prose: no call, index,
/// template or path; not an ENV_NAME; and either letters with digits or long and varied.
fn secret_value(v: &str) -> bool {
    if v.starts_with("[REDACTED")
        || v.contains(['(', ')', '[', ']', '{', '}', '<', '>', '$'])
        || code_like(v)
        || path_like(v)
    {
        return false;
    }
    let env_name = v
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if env_name {
        return false;
    }
    let letters = v.chars().any(|c| c.is_alphabetic());
    let digits = v.chars().any(|c| c.is_ascii_digit());
    // A separator inside a word (`hunter_two`) is how people write passphrases; plain prose
    // (`required`) has none.
    let separator = v.contains(['_', '-']);
    (letters && digits) || separator || (v.chars().count() >= 16 && entropy(v) >= 3.0)
}

/// The value after a command-line flag such as `--password`: anything that is not another
/// flag, a variable or code.
fn flag_value(v: &str) -> bool {
    !v.starts_with(['-', '$']) && !v.starts_with("[REDACTED") && !v.contains(['(', ')', '{', '}'])
}

fn high_entropy(v: &str) -> bool {
    !v.starts_with("[REDACTED") && !code_like(v) && !path_like(v) && entropy(v) >= HIGH_ENTROPY
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
            r"(?-u:\b)eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
            any,
        ),
        d(
            "aws_access_key",
            r"(?-u:\b)(?:AKIA|ASIA)[0-9A-Z]{16}(?-u:\b)",
            0,
            any,
        ),
        d(
            "aws_secret_key",
            r#"(?i)(?-u:\b)aws_?secret_?(?:access_?)?key(?-u:\b)\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})"#,
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
            r"(?-u:\b)(?:(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})(?-u:\b)",
            0,
            any,
        ),
        d("gitlab_token", r"(?-u:\b)glpat-[A-Za-z0-9_\-]{20,}", 0, any),
        d(
            "slack_token",
            r"(?-u:\b)xox[abprs]-[A-Za-z0-9-]{10,}",
            0,
            any,
        ),
        d(
            "stripe_key",
            r"(?-u:\b)(?:sk|rk)_live_[A-Za-z0-9]{16,}",
            0,
            any,
        ),
        d(
            "sendgrid_key",
            r"(?-u:\b)SG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}",
            0,
            any,
        ),
        d(
            "vault_token",
            r"(?-u:\b)hv[sbr]\.[A-Za-z0-9_-]{20,}",
            0,
            any,
        ),
        d(
            "anthropic_key",
            r"(?-u:\b)sk-ant-[A-Za-z0-9_\-]{20,}",
            0,
            any,
        ),
        d(
            "openai_key",
            r"(?-u:\b)sk-(?:proj-|svcacct-)?[A-Za-z0-9_\-]{20,}",
            0,
            any,
        ),
        d(
            "url_credentials",
            r"(?-u:\b)[A-Za-z][A-Za-z0-9+.\-]*://[^\s/:@\[\]]+:([^\s/@\[\]]+)@",
            1,
            any,
        ),
        d(
            "password_assignment",
            r#"(?i)(?:[A-Za-z0-9_]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|auth[_-]?token|client[_-]?secret)|รหัสผ่าน)["']?\s*[:=]\s*["']?([^\s"',;]{6,})"#,
            1,
            secret_value,
        ),
        d(
            "password_assignment",
            r#"(?i)(?:^|[\s"'`(\[,])--(?:password|passwd|token|secret|api-key|client-secret)["']?(?:\s*,\s*|\s+|=)["']?([^\s"',;)\]`]{6,})"#,
            1,
            flag_value,
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

    /// The detectors for `p`: the built-in ones, then its custom patterns. A custom pattern
    /// that does not compile or that matches empty text is skipped (with a warning that names
    /// it, never its expression) and the others stay: one bad pattern must not switch off the
    /// rest. A bad allowlist entry is skipped too, which keeps fewer matches (the safe side).
    /// Settings validation refuses both before they get here; this is the second line.
    pub fn new(p: &PrivacySettings) -> Result<Self> {
        let mut detectors = builtin_detectors();
        for c in &p.custom_patterns {
            match Regex::new(&c.regex) {
                Ok(re) if !re.is_match("") => detectors.push(Detector {
                    name: format!("custom:{}", c.name),
                    re,
                    group: 0,
                    check: any,
                }),
                Ok(_) => tracing::warn!(
                    pattern = %c.name,
                    "privacy.custom_patterns matches empty text and is skipped"
                ),
                Err(_) => tracing::warn!(
                    pattern = %c.name,
                    "privacy.custom_patterns is not a valid regular expression and is skipped"
                ),
            }
        }
        let allow = p
            .allowlist
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match Regex::new(&format!("^(?:{a})$")) {
                Ok(re) => Some(re),
                Err(_) => {
                    tracing::warn!(
                        index = i,
                        "privacy.allowlist entry is invalid and is skipped"
                    );
                    None
                }
            })
            .collect();
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
        let mut out: std::borrow::Cow<'_, str> = std::borrow::Cow::Borrowed(text);
        let mut found: Vec<String> = Vec::new();
        if !self.enabled {
            return Redaction {
                text: text.to_string(),
                found,
            };
        }
        for d in &self.detectors {
            let tag = format!("[REDACTED:{}]", d.name);
            // Allocated on the first hit only: most text matches no detector.
            let mut result: Option<String> = None;
            let mut last = 0;
            for caps in d.re.captures_iter(&out) {
                let Some(m) = caps.get(d.group) else { continue };
                let value = m.as_str();
                if value.is_empty()
                    || !(d.check)(value)
                    || self.allow.iter().any(|a| a.is_match(value))
                {
                    continue;
                }
                let r = result.get_or_insert_with(|| String::with_capacity(out.len()));
                r.push_str(&out[last..m.start()]);
                r.push_str(&tag);
                last = m.end();
            }
            if let Some(mut r) = result {
                r.push_str(&out[last..]);
                out = std::borrow::Cow::Owned(r);
                if !found.contains(&d.name) {
                    found.push(d.name.clone());
                }
            }
        }
        Redaction {
            text: out.into_owned(),
            found,
        }
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
            return Err(AliceError::Refused {
                field: field.to_string(),
                detectors: r.found,
            });
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
        ("password = hunter_two", "password_assignment", "hunter_two"),
        (
            "password = correct_horse_battery_staple",
            "password_assignment",
            "correct_horse_battery_staple",
        ),
        (
            "password = my_pass1_ok",
            "password_assignment",
            "my_pass1_ok",
        ),
        ("API_KEY=prod_key_v2", "password_assignment", "prod_key_v2"),
        (
            "password = my_password_field_name_v2",
            "password_assignment",
            "my_password_field_name_v2",
        ),
        ("password = ~hunter22", "password_assignment", "~hunter22"),
        (
            "password = /Zm9vYmFyYmF6cXV4MTIzNDU2",
            "password_assignment",
            "/Zm9vYmFyYmF6cXV4MTIzNDU2",
        ),
        (
            "SECRET_KEY=/9j/4AAQSkZJRgABAQEASABIAAD/2wBDAAYEBQYFBAYGBQYHBwYIChAKCgkJChQODwwQFxQYGBcUFhYaHSUfGhsjHBYWICwgIyYnKSopGR8tMC0oMCUoKSj/2wBDAQcHBwoIChMKChMoGhYaKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCj/wAARCAAB",
            "high_entropy",
            "4AAQSkZJRgABAQEASABIAAD",
        ),
        (
            "เชื่อมpostgres://admin:s3cr3tPass@db:5432/app",
            "url_credentials",
            "s3cr3tPass",
        ),
        (
            "ใช้aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "aws_secret_key",
            "wJalrXUtnFEMI/K7MDENG",
        ),
        (
            "bash run.sh --password hunter2xyz",
            "password_assignment",
            "hunter2xyz",
        ),
        (
            "curl --token=abc123def456ghi",
            "password_assignment",
            "abc123def456ghi",
        ),
        (
            "x --client-secret Zm9vYmFyYmF6",
            "password_assignment",
            "Zm9vYmFyYmF6",
        ),
        ("password = ./hunter22", "password_assignment", "./hunter22"),
        (
            "{\"password\": \"hunter_two\"}",
            "password_assignment",
            "hunter_two",
        ),
        (
            "\"api_key\": \"prod_key_v2\"",
            "password_assignment",
            "prod_key_v2",
        ),
        (
            "password = /9j/4AAQSkZJRgABAQEASABIAAD/2wBDAAYEBQYFBAYGBQYHBwYIChAK",
            "password_assignment",
            "4AAQSkZJRgABAQEASABIAAD",
        ),
        (
            "deploy --api-key abc123def456",
            "password_assignment",
            "abc123def456",
        ),
        (
            "`--password hunter2xyz`",
            "password_assignment",
            "hunter2xyz",
        ),
        (
            "(--password hunter2xyz)",
            "password_assignment",
            "hunter2xyz",
        ),
        (
            "[\"--password\", \"hunter2xyz\"]",
            "password_assignment",
            "hunter2xyz",
        ),
        (
            concat!("mail SG", ".abcdefghijklmnop1234.ABCDEFGHIJKLMNOP5678-_x"),
            "sendgrid_key",
            "abcdefghijklmnop1234",
        ),
        (
            concat!("login hv", "s.CAESIJabcdefghijklmnopqrstuvwxyz0123"),
            "vault_token",
            "CAESIJabcdefghijklmnopqrstuvwxyz0123",
        ),
        (
            "auth_token = hunter2.hunter2.hunter2",
            "password_assignment",
            "hunter2.hunter2.hunter2",
        ),
        (
            concat!(
                "TFE_TOKEN=abcdefghijklmn.atlas",
                "v1.Zm9vYmFyYmF6cXV4MTIzNDU2Zm9vYmFyYmF6cXV4MTIzNDU2"
            ),
            "password_assignment",
            "Zm9vYmFyYmF6cXV4MTIzNDU2",
        ),
        (
            "DOPPLER_TOKEN=dp.st.prod.Zm9vYmFyYmF6cXV4MTIzNDU2Zm9vYmFy",
            "password_assignment",
            "Zm9vYmFyYmF6cXV4MTIzNDU2",
        ),
        (
            "DB_PASSWORD=hunter2hunter2",
            "password_assignment",
            "hunter2hunter2",
        ),
        (
            "MY_API_KEY=abc123def456",
            "password_assignment",
            "abc123def456",
        ),
        (
            "SLACK_SIGNING_SECRET=abcdef0123456789",
            "password_assignment",
            "abcdef0123456789",
        ),
        (
            "export GITHUB_TOKEN=notaprefixedtoken12345",
            "password_assignment",
            "notaprefixedtoken12345",
        ),
        (
            "{\"password\": \"hunter2xyz\"}",
            "password_assignment",
            "hunter2xyz",
        ),
        (
            "\"client_secret\": \"Zm9vYmFyYmF6cXV4MTIzNDU2\"",
            "password_assignment",
            "Zm9vYmFyYmF6cXV4MTIzNDU2",
        ),
        (
            concat!("คีย์AKIA", "IOSFODNN7EXAMPLE"),
            "aws_access_key",
            concat!("AKIA", "IOSFODNN7EXAMPLE"),
        ),
        (
            concat!("ใช้sk-", "ant-api03-abcdefghijklmnopqrstuvwxyz012345"),
            "anthropic_key",
            concat!("sk-", "ant-api03-abcdef"),
        ),
        (
            concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789ใช้ได้"),
            "github_token",
            concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
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
        "PWD=/Users/icue/Code/AI/project-alice",
        "token = get_token_from_cache_or_refresh",
        "api_key = get_api_key_from_environment_variable",
        "secret = ~/.config/alice/secret-file",
        "token = ./relative/token_path",
        "password = /Users/me/project",
        "secret = ~/.ssh/config",
        "token = ./build/out",
        "PWD=/Users/icue/Code/MyCoolProject",
        "token = self.token",
        "secret = process.env.SECRET_KEY",
        "api_key = os.environ",
        "run --password-file ./secrets/pw --token-file ~/.t",
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

    /// One bad custom pattern (invalid, or matching empty text) is skipped; the good ones and the
    /// mode stay in force.
    #[test]
    fn a_bad_pattern_is_skipped_alone_and_the_fallback_still_redacts() {
        let p = PrivacySettings {
            custom_patterns: vec![
                CustomPattern {
                    name: "broken".into(),
                    regex: "(unclosed-TCK".into(),
                },
                CustomPattern {
                    name: "empty".into(),
                    regex: ".*".into(),
                },
                CustomPattern {
                    name: "digits".into(),
                    regex: r"\d*".into(),
                },
                CustomPattern {
                    name: "ticket".into(),
                    regex: "TCK-[0-9]{4,}".into(),
                },
            ],
            allowlist: vec!["(".into()],
            redact_mode: RedactMode::Block,
            ..Default::default()
        };
        let r = Redactor::new(&p).unwrap();
        assert!(r.blocks());
        let e = r.check("content", "see TCK-12345").unwrap_err();
        assert!(
            matches!(&e, AliceError::Refused { field, detectors }
                if field == "content" && detectors == &["custom:ticket"]),
            "{e:?}"
        );
        assert!(is_refusal(&e) && !is_refusal(&AliceError::invalid("x")));
        assert_eq!(
            r.apply("nothing 123 here").found.len(),
            0,
            "empty patterns do not fire"
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
    fn text_without_a_secret_comes_back_identical() {
        let r = Redactor::builtin();
        let out = r.apply("plain prose, nothing to see");
        assert_eq!(
            (out.text.as_str(), out.found.len()),
            ("plain prose, nothing to see", 0)
        );
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
