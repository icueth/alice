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
    /// A built-in detector, not a custom pattern: text that leaves for a cloud service is
    /// redacted by it whatever the allowlist says ([`Redactor::for_egress`]).
    builtin: bool,
}

/// A compiled set of detectors. Build it once and share it; compiling costs milliseconds.
pub struct Redactor {
    enabled: bool,
    mode: RedactMode,
    detectors: Vec<Detector>,
    allow: Vec<Regex>,
    /// Whether the allowlist exempts a built-in detector's match (not for egress).
    allow_builtin: bool,
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

/// A value that points at a secret kept elsewhere rather than being one, judged on the whole
/// value: a variable (`$NAME`, `${NAME}`, a shell substitution `$(…)`), a placeholder
/// (`<name>`, `{name}`, `{{ … }}`), or code that calls, indexes or names a generic type
/// (`get(…)`, `env[…]`, `Option<…>`). A `$`, a bracket or `<>` inside a value does not make it
/// one: generated passwords hold them (`Pa$$w0rd`, `Tr0ub4dor&3(x)`).
fn reference(v: &str) -> bool {
    static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"^(?:\$\{?[A-Za-z_][A-Za-z0-9_]*\}?|\$\(.*|<[^<>]*>?|\{[A-Za-z0-9_.]*\}|\{\{.*|[A-Za-z0-9_.:]+[(\[<].*)$",
        )
        .expect("built-in pattern")
    });
    RE.is_match(v)
}

/// A value that is code and not a secret: a reference (see [`reference`]), a dotted attribute
/// access, or a call to an accessor by name. A plain `snake_case` word is not evidence of code:
/// passphrases look like that, so redaction errs toward redacting it.
fn code_like(v: &str) -> bool {
    reference(v) || attribute_access(v) || ACCESSOR_PREFIXES.iter().any(|a| v.starts_with(a))
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

/// A value after `password =` etc. that is a secret, not code or prose: not a reference, code
/// or a path; not an ENV_NAME; and either letters with digits or long and varied.
fn secret_value(v: &str) -> bool {
    if v.starts_with("[REDACTED") || code_like(v) || path_like(v) {
        return false;
    }
    // An ENV_NAME has no digit (`DATABASE_URL`), or words joined by `_` (`MY_VAR_2`); upper
    // case with digits and no `_` is how many keys look (`7KQ2M9X4P8R3T6W1`).
    let upper = v
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let inner_underscore = v.trim_matches('_').contains('_');
    if upper && (!v.chars().any(|c| c.is_ascii_digit()) || inner_underscore) {
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
    !v.starts_with('-') && !v.starts_with("[REDACTED") && !reference(v)
}

/// Base64 lines, not a list of hex digests (a key's body is never hex only).
fn not_hex_only(v: &str) -> bool {
    v.chars()
        .any(|c| !c.is_ascii_hexdigit() && !c.is_whitespace())
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
        builtin: true,
    };
    vec![
        d(
            "pem_private_key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
            0,
            any,
        ),
        // The tail of a key whose `BEGIN` line is in an earlier piece (a chunk cut by size):
        // the text starts with its base64 lines and ends the key. A rendered chunk's tag
        // (`[user] (cont.) `) may come first; it is kept.
        d("pem_private_key", KEY_TAIL, 1, any),
        // A key's body cut from both its lines: three or more base64 lines of a key's width.
        // The first may follow a rendered chunk's tag (`[user] `, `[user] (cont.) `), which is
        // kept: a chunk that starts with a key's lines would otherwise send its first line.
        d(
            "base64_block",
            r"(?m)^(?:\[[^\]\n]+\] (?:\(cont\.\) )?)?((?:[ \t]*[A-Za-z0-9+/]{60,76}={0,2}[ \t]*\r?(?:\n|\z)){3,})",
            1,
            not_hex_only,
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

/// Base64 lines (and the headers of an encrypted key) at the very start of a text, after the
/// tag of a rendered chunk if it has one, then the `END` line of a private key (group 1).
const KEY_TAIL: &str = r"\A(?:\[[^\]\n]+\] (?:\(cont\.\) )?)?(\s*(?:(?:Proc-Type|DEK-Info):[^\n]*\n\s*)*(?:[A-Za-z0-9+/=]+[ \t]*\r?\n\s*)*[A-Za-z0-9+/=]*\s*-----END [A-Z ]*PRIVATE KEY-----)";

/// What a piece that continues a private key starts with: the tag of a rendered chunk if it has
/// one (group 1, kept), then the key's base64 lines (and the headers of an encrypted key),
/// through its `END` line when it is there.
static KEY_CONTINUED: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\A((?:\[[^\]\n]+\] (?:\(cont\.\) )?)?)\s*(?:(?:Proc-Type|DEK-Info):[^\n]*(?:\n\s*|\z))*(?:[A-Za-z0-9+/=]+[ \t]*(?:\r?\n\s*|\z))*(?:-----END [A-Z ]*PRIVATE KEY-----)?")
        .expect("built-in pattern")
});

/// The key lines after a private key's `BEGIN` marker: the rest of its line, then every line
/// that holds base64 only (or an encrypted key's header, or nothing). Where a key with no `END`
/// line ends for [`Redactor::spans`].
static KEY_LINES: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?m)\A[^\n]*(?:\n[ \t]*(?:(?:Proc-Type|DEK-Info):[^\n]*|[A-Za-z0-9+/=]*[ \t]*\r?)$)*",
    )
    .expect("built-in pattern")
});

static KEY_LINE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"-----(BEGIN|END) [A-Z ]*PRIVATE KEY-----").expect("built-in pattern")
});

/// Whether `text` ends inside a private key block: its last `BEGIN … PRIVATE KEY` line has no
/// `END` line after it.
pub fn ends_inside_key(text: &str) -> bool {
    KEY_LINE
        .captures_iter(text)
        .last()
        .is_some_and(|c| &c[1] == "BEGIN")
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
            allow_builtin: true,
        }
    }

    /// The built-in detectors with the default settings, compiled once for the process: what
    /// finds secrets where no project's rules are at hand (a derived title, a capture read
    /// without a redactor).
    pub fn builtin_shared() -> &'static Self {
        static BUILTIN: std::sync::LazyLock<Redactor> = std::sync::LazyLock::new(Redactor::builtin);
        &BUILTIN
    }

    /// This redactor for text that leaves for a cloud service (re-review N3): switched on
    /// whatever `privacy.redact` says, and its allowlist no longer exempts a built-in
    /// detector's match (it still exempts a custom pattern's). A person who keeps secrets as
    /// written, or who allowlisted every value, still sends no key, password or private key of
    /// a kind Alice knows.
    ///
    /// ```
    /// use alice_core::redact::Redactor;
    /// use alice_core::settings::PrivacySettings;
    ///
    /// let p = PrivacySettings {
    ///     redact: false,
    ///     allowlist: vec!["(?s).*".into()],
    ///     ..Default::default()
    /// };
    /// let text = "password = hunter22x";
    /// assert_eq!(Redactor::new(&p).unwrap().apply(text).text, text);
    /// let egress = Redactor::new(&p).unwrap().for_egress();
    /// assert_eq!(egress.apply(text).text, "password = [REDACTED:password_assignment]");
    /// ```
    #[must_use]
    pub fn for_egress(mut self) -> Self {
        self.enabled = true;
        self.allow_builtin = false;
        self
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
                    builtin: false,
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
            allow_builtin: true,
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
                if value.is_empty() || !(d.check)(value) || self.allowed(d, value) {
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

    /// Whether the allowlist exempts `value`, a match of `d`.
    fn allowed(&self, d: &Detector, value: &str) -> bool {
        (self.allow_builtin || !d.builtin) && self.allow.iter().any(|a| a.is_match(value))
    }

    /// The byte ranges of `text` that hold a secret, as the detectors find it in this text:
    /// each match whole (a keyword or a URL's scheme and user name with the value they
    /// introduce, a private key from its `BEGIN` line), sorted, overlapping ones merged. A key
    /// with no `END` line spans its `BEGIN` line and the key lines after it, not the rest of
    /// the text.
    ///
    /// It ignores the switch and the allowlist. Capture asks it in every mode, so that no trim
    /// or cut falls inside a secret (re-review N1): a cut that separates a value from the
    /// context its detector needs (`:Thailand-2026@db.internal` without its `postgres://ops`)
    /// leaves text no detector knows, and a cloud request redacts each stored chunk on its own.
    ///
    /// ```
    /// use alice_core::redact::Redactor;
    ///
    /// let text = concat!("url postgres://ops:Thai", "land-2026@db.internal:5432/app ok");
    /// let spans = Redactor::builtin().spans(text);
    /// assert_eq!(spans.len(), 1);
    /// assert_eq!(&text[spans[0].clone()], concat!("postgres://ops:Thai", "land-2026@"));
    /// ```
    pub fn spans(&self, text: &str) -> Vec<std::ops::Range<usize>> {
        let mut found: Vec<std::ops::Range<usize>> = Vec::new();
        for d in &self.detectors {
            for caps in d.re.captures_iter(text) {
                let (Some(whole), Some(value)) = (caps.get(0), caps.get(d.group)) else {
                    continue;
                };
                if value.as_str().is_empty() || !(d.check)(value.as_str()) {
                    continue;
                }
                let mut range = whole.range();
                let matched = whole.as_str();
                if matched.starts_with("-----BEGIN")
                    && !matched.contains("-----END")
                    && let Some(marker) = KEY_LINE.find_at(text, range.start)
                {
                    let after = &text[marker.end()..];
                    let lines = KEY_LINES.find(after).map_or(0, |m| m.end());
                    let lines = after[..lines].trim_end().len();
                    range.end = range.end.min(marker.end() + lines).max(marker.end());
                }
                if !range.is_empty() {
                    found.push(range);
                }
            }
        }
        found.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(found.len());
        for r in found {
            match merged.last_mut() {
                Some(last) if r.start < last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
    }

    /// [`apply`](Self::apply) for the next of consecutive pieces of one text (the chunks of one
    /// ingest, in order). `open` says whether the piece before ended inside a private key block
    /// (see [`ends_inside_key`]); the base64 lines this piece then starts with, through the
    /// key's `END` line, are the rest of that key and are redacted too. `open` is updated for
    /// the piece after this one.
    ///
    /// ```
    /// use alice_core::redact::Redactor;
    ///
    /// let r = Redactor::builtin();
    /// let mut open = false;
    /// let first = r.apply_piece(concat!("key:\n-----BEGIN RSA PRIVATE ", "KEY-----\nMIIE"), &mut open);
    /// assert!(open && !first.text.contains("MIIE"));
    /// let second = r.apply_piece("pAIBAAKCAQEA1234\nzzzz\n-----END RSA PRIVATE KEY-----\nafter", &mut open);
    /// assert_eq!(second.text, "[REDACTED:pem_private_key]\nafter");
    /// assert!(!open);
    /// ```
    pub fn apply_piece(&self, text: &str, open: &mut bool) -> Redaction {
        if !self.enabled {
            *open = ends_inside_key(text);
            return self.apply(text);
        }
        // A rendered chunk's tag (`[user] (cont.) `) is kept in front of the key's rest.
        let continued = if *open {
            KEY_CONTINUED.captures(text)
        } else {
            None
        };
        let (lead, head) = match continued {
            Some(c) => (
                c.get(1).map_or(0, |m| m.end()),
                c.get(0).map_or(0, |m| m.end()),
            ),
            None => (0, 0),
        };
        let (tag, key, rest) = (&text[..lead], &text[lead..head], &text[head..]);
        let whole = head == text.len();
        *open = (whole && !key.contains("-----END")) || ends_inside_key(rest);
        let mut out = self.apply(rest);
        if !key.trim().is_empty() {
            let name = "pem_private_key".to_string();
            out.text = format!("{tag}[REDACTED:{name}]{}", out.text);
            if !out.found.contains(&name) {
                out.found.insert(0, name);
            }
        } else {
            out.text = format!("{tag}{key}{}", out.text);
        }
        out
    }

    /// [`check`](Self::check) for the next of consecutive pieces of one text (see
    /// [`apply_piece`](Self::apply_piece)).
    pub fn check_piece(&self, field: &str, text: &str, open: &mut bool) -> Result<String> {
        let r = self.apply_piece(text, open);
        if self.blocks() && !r.found.is_empty() {
            return Err(AliceError::Refused {
                field: field.to_string(),
                detectors: r.found,
            });
        }
        Ok(r.text)
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
        // Final review A-I2: `$`, brackets and upper-case keys are values too.
        (
            "DB_PASSWORD=Pa$$w0rd2024",
            "password_assignment",
            "Pa$$w0rd2024",
        ),
        (
            "\"password\": \"Tr0ub4dor&3(x)\"",
            "password_assignment",
            "Tr0ub4dor&3(x)",
        ),
        (
            "API_KEY=7KQ2M9X4P8R3T6W1",
            "password_assignment",
            "7KQ2M9X4P8R3T6W1",
        ),
        (
            "password: HUNTER2HUNTER2",
            "password_assignment",
            "HUNTER2HUNTER2",
        ),
        (
            "mysql --password 'Tr0ub4dor&3(x)'",
            "password_assignment",
            "Tr0ub4dor&3(x)",
        ),
        ("token = <x7Kq9>Zz!2", "password_assignment", "<x7Kq9>Zz!2"),
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
        "password = $DB_PASSWORD",
        "token: ${GITHUB_TOKEN}",
        "API_KEY=$(vault_read api)",
        "password = <your-password>",
        "token: Option<Arc<str>>,",
        "password = {password}",
        "secret = data[\"secret\"]",
        "password = MY_VAR_2",
        "token = client.get_token(scope)",
        "--password $PGPASSWORD",
        concat!(
            "the file must end with -----END RSA PRIVATE ",
            "KEY----- and a newline"
        ),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
         e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
         e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ];

    /// A made-up PEM key of `lines` base64 lines of 64 characters, and those lines.
    fn fake_key(lines: usize) -> (String, Vec<String>) {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let body: Vec<String> = (0..lines)
            .map(|_| {
                (0..64)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        alphabet[(x % 64) as usize] as char
                    })
                    .collect()
            })
            .collect();
        let text = format!(
            "{}\n{}\n-----END RSA PRIVATE KEY-----",
            concat!("-----BEGIN RSA PRIVATE ", "KEY-----"),
            body.join("\n")
        );
        (text, body)
    }

    /// Any 12 characters in a row of the key's body found in `text`.
    fn key_leaks(text: &str, body: &[String]) -> Vec<String> {
        body.iter()
            .flat_map(|l| (0..=l.len() - 12).map(move |i| &l[i..i + 12]))
            .filter(|w| text.contains(w))
            .map(String::from)
            .collect()
    }

    /// Final review A-I1: a key cut by size across the chunks of one ingest (even mid-line)
    /// leaves nothing of its body in any chunk.
    #[test]
    fn a_private_key_cut_across_pieces_is_redacted_in_every_piece() {
        let r = Redactor::builtin();
        let (key, body) = fake_key(26);
        let text = format!("notes before\n{key}\nnotes after");
        let start = text.find(&body[0]).unwrap();
        let len = body.len() * 65;
        for pieces in [2usize, 3, 4] {
            for shift in [0usize, 7, 31, 64] {
                let mut cuts: Vec<usize> = (1..pieces)
                    .map(|k| start + len * k / pieces + shift)
                    .collect();
                cuts.dedup();
                let mut parts = Vec::new();
                let mut from = 0;
                for c in cuts.iter().copied().chain([text.len()]) {
                    parts.push(&text[from..c]);
                    from = c;
                }
                let mut open = false;
                let out: Vec<String> = parts
                    .iter()
                    .map(|p| r.apply_piece(p, &mut open).text)
                    .collect();
                let all = out.join("|");
                assert!(
                    key_leaks(&all, &body).is_empty(),
                    "{pieces} pieces, shift {shift}: {all}"
                );
                assert!(
                    all.starts_with("notes before") && all.ends_with("notes after"),
                    "{all}"
                );
                assert!(!open, "the key ended");
            }
        }
        // A piece after one that did not end inside a key is redacted as usual.
        let mut open = false;
        assert_eq!(r.apply_piece("ZZZZ plain", &mut open).text, "ZZZZ plain");
        // Text that only mentions a key opens nothing that eats the next piece's prose.
        let mut open = false;
        r.apply_piece(
            concat!("paste the -----BEGIN RSA PRIVATE ", "KEY----- line"),
            &mut open,
        );
        assert!(open);
        assert_eq!(r.apply_piece("then run it", &mut open).text, "then run it");
        assert!(!open);
    }

    /// Final review A-I1: a key's tail (its last base64 lines and its `END` line) and a body cut
    /// from both its lines are caught in a piece on its own, too.
    #[test]
    fn the_tail_and_the_body_of_a_cut_key_are_caught_on_their_own() {
        let r = Redactor::builtin();
        let (_, body) = fake_key(26);
        let middle = body[3..12].join("\n");
        let out = r.apply(&format!("{middle}\nand prose"));
        assert_eq!(out.found, ["base64_block"]);
        assert!(key_leaks(&out.text, &body[3..12]).is_empty() && out.text.ends_with("and prose"));
        let tail = format!(
            "{}\n-----END RSA PRIVATE KEY-----\nafter",
            body[20..].join("\n")
        );
        let out = r.apply(&tail);
        assert!(
            out.found.contains(&"pem_private_key".to_string()),
            "{:?}",
            out.found
        );
        assert!(key_leaks(&out.text, &body[20..]).is_empty(), "{}", out.text);
        assert!(out.text.ends_with("\nafter"));
        // A tail cut mid-line starts with part of a line.
        let cut = format!(
            "{}\n-----END RSA PRIVATE KEY-----",
            &body.join("\n")[64 * 24 + 30..]
        );
        assert!(key_leaks(&r.apply(&cut).text, &body[25..]).is_empty());
    }

    /// Re-review N1: a stored chunk starts with its tag (`[user] `, `[user] (cont.) `), which
    /// used to hide a key's first line from the line-anchored detectors. The tag is kept.
    #[test]
    fn a_chunk_tag_in_front_of_a_key_hides_none_of_it() {
        let r = Redactor::builtin();
        let (_, body) = fake_key(26);
        for tag in ["[user] ", "[user] (cont.) ", "[result] (cont.) "] {
            let lines = format!("{tag}{}\nand prose", body[3..6].join("\n"));
            let out = r.apply(&lines);
            assert_eq!(out.found, ["base64_block"], "{tag}");
            assert!(key_leaks(&out.text, &body[3..6]).is_empty(), "{}", out.text);
            assert!(out.text.starts_with(tag) && out.text.ends_with("and prose"));

            let tail = format!(
                "{tag}{}\n-----END RSA PRIVATE KEY-----\nafter",
                body[22..].join("\n")
            );
            let out = r.apply(&tail);
            assert!(key_leaks(&out.text, &body[22..]).is_empty(), "{}", out.text);
            assert!(
                out.text
                    .starts_with(&format!("{tag}[REDACTED:pem_private_key]"))
            );

            // As the next of two pieces, after one that ends inside the key.
            let mut open = true;
            let out = r.apply_piece(&format!("{tag}{}", body[24..].join("\n")), &mut open);
            assert!(key_leaks(&out.text, &body[24..]).is_empty(), "{}", out.text);
            assert_eq!(out.text, format!("{tag}[REDACTED:pem_private_key]"));
            assert!(open, "the key goes on");
        }
    }

    #[test]
    fn spans_cover_each_secret_with_its_context() {
        let r = Redactor::builtin();
        let text = concat!(
            "db postgres://ops:Thai",
            "land-2026@db.internal and\n  password: VGhh",
            "aWxhbmQtMjAy\nkey -----BEGIN RSA PRIVATE ",
            "KEY-----\nMIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\n",
            "-----END RSA PRIVATE KEY----- done"
        );
        let got: Vec<&str> = r.spans(text).into_iter().map(|s| &text[s]).collect();
        assert_eq!(got.len(), 3, "{got:#?}");
        assert!(got[0].starts_with("postgres://ops:") && got[0].ends_with('@'));
        assert!(got[1].starts_with("password: ") && got[1].ends_with("MjAy"));
        assert!(got[2].starts_with("-----BEGIN") && got[2].ends_with("PRIVATE KEY-----"));
        // The switch and the allowlist do not hide a secret from the cuts.
        let off = Redactor::new(&PrivacySettings {
            redact: false,
            allowlist: vec!["(?s).*".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(off.spans(text), r.spans(text));
        // Code and prose hold none.
        assert!(
            r.spans("let password = get_password(); see /usr/lib/x")
                .is_empty()
        );
    }

    /// Re-review N3: the allowlist is an exception for storage; egress keeps a built-in
    /// detector's match redacted and only a custom pattern's exception.
    #[test]
    fn egress_keeps_the_allowlist_for_custom_patterns_only() {
        let p = PrivacySettings {
            allowlist: vec!["(?s).*".into()],
            custom_patterns: vec![CustomPattern {
                name: "ticket".into(),
                regex: "TCK-[0-9]{4,}".into(),
            }],
            ..Default::default()
        };
        let text = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 TCK-1234");
        assert_eq!(Redactor::new(&p).unwrap().apply(text).text, text);
        let out = Redactor::new(&p).unwrap().for_egress().apply(text);
        assert_eq!(out.found, ["github_token"]);
        assert!(out.text.ends_with(" TCK-1234"));
    }

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

    /// Final review A-I2: a value is a reference only when the whole value is one; an upper-case
    /// value with digits and no `_` is a key, not an ENV_NAME.
    #[test]
    fn passwords_with_dollars_brackets_and_upper_case_keys_are_redacted() {
        let r = Redactor::builtin();
        for (input, secret) in [
            ("DB_PASSWORD=Pa$$w0rd2024", "Pa$$w0rd2024"),
            ("\"password\": \"Tr0ub4dor&3(x)\"", "Tr0ub4dor&3(x)"),
            ("API_KEY=7KQ2M9X4P8R3T6W1", "7KQ2M9X4P8R3T6W1"),
            ("password: HUNTER2HUNTER2", "HUNTER2HUNTER2"),
        ] {
            let out = r.apply(input);
            assert_eq!(out.found, ["password_assignment"], "{input}");
            assert!(!out.text.contains(secret), "{input} -> {}", out.text);
        }
        for reference in [
            "$DB_PASSWORD",
            "${DB_PASSWORD}",
            "<your-password>",
            "{password}",
            "Option<String>",
            "env[\"X\"]",
            "get_secret(x)",
        ] {
            assert!(super::code_like(reference), "{reference}");
        }
        assert!(!super::code_like("Tr0ub4dor&3(x)") && !super::code_like("Pa$$w0rd2024"));
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
