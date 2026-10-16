//! A small JSON value that keeps object keys in file order (plan decision P5). The agents'
//! settings files are edited through it, so a person's own keys stay where they were:
//! `serde_json` sorts keys unless its `preserve_order` feature is on, and that feature would
//! change JSON output across the workspace.
//!
//! Strict JSON (RFC 8259): no comments, no trailing commas, no duplicate keys. A number keeps
//! the text it was written with. [`print`] writes 2-space indentation and a trailing newline, as
//! Claude Code and Codex write these files.

/// A JSON value whose objects keep their keys in order.
#[derive(Debug, Clone, PartialEq)]
pub enum JValue {
    Null,
    Bool(bool),
    /// The number as written (`1e3` stays `1e3`).
    Num(String),
    Str(String),
    Arr(Vec<JValue>),
    Obj(Vec<(String, JValue)>),
}

/// How deep arrays and objects may nest (serde_json's limit too).
const MAX_DEPTH: usize = 128;

impl JValue {
    /// The value of `key`, for an object that has it.
    pub fn get(&self, key: &str) -> Option<&JValue> {
        match self {
            JValue::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut JValue> {
        match self {
            JValue::Obj(fields) => fields.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The same value as `serde_json` holds it (keys sorted). Fails on a number `serde_json`
    /// cannot hold (`1e400`).
    pub fn to_serde(&self) -> Result<serde_json::Value, String> {
        use serde_json::Value;
        Ok(match self {
            JValue::Null => Value::Null,
            JValue::Bool(b) => Value::Bool(*b),
            JValue::Num(n) => Value::Number(
                n.parse()
                    .map_err(|e| format!("the number {n} cannot be read back ({e})"))?,
            ),
            JValue::Str(s) => Value::String(s.clone()),
            JValue::Arr(items) => Value::Array(
                items
                    .iter()
                    .map(JValue::to_serde)
                    .collect::<Result<_, _>>()?,
            ),
            JValue::Obj(fields) => {
                let mut map = serde_json::Map::new();
                for (k, v) in fields {
                    map.insert(k.clone(), v.to_serde()?);
                }
                Value::Object(map)
            }
        })
    }
}

/// Parses strict JSON. The error names the line and column, never the text there.
pub fn parse(text: &str) -> Result<JValue, String> {
    let mut p = Parser {
        text,
        chars: text.char_indices().peekable(),
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.peek().is_some() {
        return Err(p.err("unexpected text after the value"));
    }
    Ok(v)
}

/// The value with 2-space indentation and a trailing newline.
pub fn print(v: &JValue) -> String {
    let mut out = String::new();
    write(&mut out, v, 0);
    out.push('\n');
    out
}

fn write(out: &mut String, v: &JValue, depth: usize) {
    let pad = |out: &mut String, depth: usize| out.push_str(&"  ".repeat(depth));
    match v {
        JValue::Null => out.push_str("null"),
        JValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JValue::Num(n) => out.push_str(n),
        JValue::Str(s) => quote(out, s),
        JValue::Arr(items) if items.is_empty() => out.push_str("[]"),
        JValue::Obj(fields) if fields.is_empty() => out.push_str("{}"),
        JValue::Arr(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                pad(out, depth + 1);
                write(out, item, depth + 1);
                out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
            }
            pad(out, depth);
            out.push(']');
        }
        JValue::Obj(fields) => {
            out.push_str("{\n");
            for (i, (k, item)) in fields.iter().enumerate() {
                pad(out, depth + 1);
                quote(out, k);
                out.push_str(": ");
                write(out, item, depth + 1);
                out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
            }
            pad(out, depth);
            out.push('}');
        }
    }
}

/// `s` as a JSON string: quotes, backslashes and control characters escaped, the rest as is.
fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    text: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
}

impl Parser<'_> {
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }

    fn bump(&mut self) -> Option<char> {
        self.chars.next().map(|(_, c)| c)
    }

    /// The byte offset of the next character (the end of the text after the last).
    fn pos(&mut self) -> usize {
        self.chars.peek().map_or(self.text.len(), |&(i, _)| i)
    }

    /// `what`, at the next character.
    fn err(&mut self, what: &str) -> String {
        let at = self.pos();
        self.err_at(at, what)
    }

    fn err_at(&self, at: usize, what: &str) -> String {
        let before = &self.text[..at];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        format!("{what} at line {line}, column {column}")
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.bump();
        }
    }

    fn value(&mut self, depth: usize) -> Result<JValue, String> {
        match self.peek() {
            None => Err(self.err("unexpected end of the text")),
            Some('{') => self.object(depth),
            Some('[') => self.array(depth),
            Some('"') => self.string().map(JValue::Str),
            Some('t') => self.word("true", JValue::Bool(true)),
            Some('f') => self.word("false", JValue::Bool(false)),
            Some('n') => self.word("null", JValue::Null),
            Some('-' | '0'..='9') => self.number(),
            Some(c) => Err(self.err(&format!("unexpected character {c:?}"))),
        }
    }

    fn word(&mut self, word: &str, v: JValue) -> Result<JValue, String> {
        let at = self.pos();
        if !self.text[at..].starts_with(word) {
            return Err(self.err("expected a value"));
        }
        for _ in word.chars() {
            self.bump();
        }
        Ok(v)
    }

    fn object(&mut self, depth: usize) -> Result<JValue, String> {
        if depth >= MAX_DEPTH {
            return Err(self.err("arrays and objects are nested too deep"));
        }
        self.bump();
        let mut fields: Vec<(String, JValue)> = Vec::new();
        self.ws();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(JValue::Obj(fields));
        }
        loop {
            self.ws();
            if self.peek() != Some('"') {
                return Err(self.err("expected a key in double quotes"));
            }
            let at = self.pos();
            let key = self.string()?;
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(self.err_at(at, &format!("duplicate key {key:?}")));
            }
            self.ws();
            if self.peek() != Some(':') {
                return Err(self.err("expected ':' after the key"));
            }
            self.bump();
            self.ws();
            let v = self.value(depth + 1)?;
            fields.push((key, v));
            self.ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    return Ok(JValue::Obj(fields));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<JValue, String> {
        if depth >= MAX_DEPTH {
            return Err(self.err("arrays and objects are nested too deep"));
        }
        self.bump();
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(JValue::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    return Ok(JValue::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.bump();
        let mut s = String::new();
        loop {
            let at = self.pos();
            match self.bump() {
                None => return Err(self.err("unexpected end of the text in a string")),
                Some('"') => return Ok(s),
                Some('\\') => match self.bump() {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('/') => s.push('/'),
                    Some('b') => s.push('\u{8}'),
                    Some('f') => s.push('\u{c}'),
                    Some('n') => s.push('\n'),
                    Some('r') => s.push('\r'),
                    Some('t') => s.push('\t'),
                    Some('u') => s.push(self.unicode(at)?),
                    _ => return Err(self.err_at(at, "invalid escape")),
                },
                Some(c) if (c as u32) < 0x20 => {
                    return Err(self.err_at(at, "control character in a string"));
                }
                Some(c) => s.push(c),
            }
        }
    }

    /// The character of a `\u` escape (its `\` at `at`), a surrogate pair taken whole.
    fn unicode(&mut self, at: usize) -> Result<char, String> {
        let lone = |p: &Self| p.err_at(at, "lone surrogate in a \\u escape");
        let hi = self.hex4(at)?;
        let code = match hi {
            0xD800..=0xDBFF => {
                if self.bump() != Some('\\') || self.bump() != Some('u') {
                    return Err(lone(self));
                }
                let lo = self.hex4(at)?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(lone(self));
                }
                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(lone(self)),
            _ => hi,
        };
        char::from_u32(code).ok_or_else(|| self.err_at(at, "invalid escape"))
    }

    fn hex4(&mut self, at: usize) -> Result<u32, String> {
        let mut n = 0;
        for _ in 0..4 {
            let d = self
                .bump()
                .and_then(|c| c.to_digit(16))
                .ok_or_else(|| self.err_at(at, "invalid escape"))?;
            n = n * 16 + d;
        }
        Ok(n)
    }

    fn number(&mut self) -> Result<JValue, String> {
        let start = self.pos();
        if self.peek() == Some('-') {
            self.bump();
        }
        match self.peek() {
            Some('0') => {
                self.bump();
            }
            Some('1'..='9') => self.digits(),
            _ => return Err(self.err("expected a digit")),
        }
        if self.peek() == Some('.') {
            self.bump();
            self.some_digits()?;
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            self.some_digits()?;
        }
        let end = self.pos();
        Ok(JValue::Num(self.text[start..end].to_string()))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some('0'..='9')) {
            self.bump();
        }
    }

    /// One digit or more.
    fn some_digits(&mut self) -> Result<(), String> {
        if !matches!(self.peek(), Some('0'..='9')) {
            return Err(self.err("expected a digit"));
        }
        self.digits();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: &str = r#"{
  "zeta": "last key first",
  "model": "opus",
  "env": {
    "QUOTE": "say \"hi\"\\ \/ \b\f\n\r\t \u0001 \u00e9 é",
    "THAI": "ภาษาไทย บันทึก",
    "EMOJI": "\ud83d\ude00 😀"
  },
  "numbers": [0, -0, 1e3, -0.5, 2.5E-3, 12345678901234567890, 10],
  "nested": {"b": [{"y": null, "x": true}, [], {}], "a": false},
  "alpha": []
}
"#;

    #[test]
    fn json_round_trips_and_keeps_key_order() {
        let v = parse(SETTINGS).unwrap();
        let printed = print(&v);
        let ours: serde_json::Value = serde_json::from_str(&printed).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(SETTINGS).unwrap();
        assert_eq!(ours, theirs, "{printed}");
        assert_eq!(v.to_serde().unwrap(), theirs);
        // Key order is the file's at every level, not sorted.
        let at = |k: &str| printed.find(&format!("\"{k}\"")).unwrap();
        assert!(at("zeta") < at("model") && at("model") < at("env") && at("env") < at("alpha"));
        assert!(at("QUOTE") < at("THAI") && at("THAI") < at("EMOJI"));
        assert!(at("y") < at("x") && at("b") < at("a"));
        // Numbers keep their text; text outside ASCII is written as itself.
        assert!(printed.contains("1e3") && printed.contains("-0.5") && printed.contains("2.5E-3"));
        assert!(printed.contains("ภาษาไทย บันทึก") && printed.contains("é"));
        // 2-space indentation, `[]` and `{}` for empty ones, a trailing newline.
        assert!(printed.starts_with("{\n  \"zeta\": \"last key first\",\n  \"model\""));
        assert!(printed.contains("\n      [],\n      {}\n"), "{printed}");
        assert!(printed.ends_with("\n  \"alpha\": []\n}\n"), "{printed}");
        // Printing is a fixed point, and agrees with serde_json's pretty printer on a file
        // whose keys are already sorted.
        assert_eq!(print(&parse(&printed).unwrap()), printed);
        let sorted = serde_json::to_string_pretty(&theirs).unwrap() + "\n";
        assert_eq!(print(&parse(&sorted).unwrap()), sorted);
        assert_eq!(print(&JValue::Obj(vec![])), "{}\n");
    }

    #[test]
    fn json_parse_is_strict_and_names_the_place() {
        for (bad, want) in [
            ("{\"a\": 1,}", "line 1, column 9"),
            ("{\n  \"a\": 1,\n}", "line 3, column 1"),
            ("[1, 2,]", "line 1"),
            ("{\"a\": 1} x", "after the value"),
            ("{\"a\" 1}", "line 1"),
            ("// c\n{}", "line 1"),
            ("{'a': 1}", "line 1"),
            ("{\"a\": 01}", "line 1"),
            ("{\"a\": 1.}", "line 1"),
            ("{\"a\": .5}", "line 1"),
            ("{\"a\": +1}", "line 1"),
            ("{\"a\": \"\\x\"}", "escape"),
            ("{\"a\": \"\\ud83d\"}", "surrogate"),
            ("{\"a\": \"\\ude00\"}", "surrogate"),
            ("{\"a\": \"tab\there\"}", "control character"),
            ("{\"a\": \"open", "end"),
            ("{\"a\": tru}", "line 1"),
            ("", "end"),
            ("{\"a\": 1, \"a\": 2}", "duplicate key \"a\""),
        ] {
            let e = parse(bad).expect_err(bad);
            assert!(e.contains(want), "{bad:?}: {e}");
        }
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(parse(&deep).unwrap_err().contains("deep"));
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
        // A number serde_json cannot hold parses here, and is refused on the way to it.
        assert!(parse("[1e400]").unwrap().to_serde().is_err());
    }
}
