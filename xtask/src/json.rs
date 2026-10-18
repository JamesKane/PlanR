//! A small JSON reader, enough for `cargo metadata`.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    /// Kept as text; callers here never do arithmetic on numbers.
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.get(key),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> &[Value] {
        match self {
            Value::Array(v) => v,
            _ => &[],
        }
    }
}

pub(crate) fn parse(s: &str) -> Result<Value, String> {
    let mut p = Parser { b: s.as_bytes(), i: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing data"));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        format!("json: {what} at byte {}", self.i)
    }

    fn ws(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", c as char)))
        }
    }

    fn lit(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("bad literal"))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.lit("true", Value::Bool(true)),
            Some(b'f') => self.lit("false", Value::Bool(false)),
            Some(b'n') => self.lit("null", Value::Null),
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                while self
                    .b
                    .get(self.i)
                    .is_some_and(|c| b"+-.eE".contains(c) || c.is_ascii_digit())
                {
                    self.i += 1;
                }
                Ok(Value::Number(
                    String::from_utf8_lossy(&self.b[start..self.i]).into_owned(),
                ))
            }
            _ => Err(self.err("expected a value")),
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.eat(b'{')?;
        let mut m = BTreeMap::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Value::Object(m));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.eat(b':')?;
            m.insert(k, self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(m));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.eat(b'[')?;
        let mut v = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Value::Array(v));
        }
        loop {
            v.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(v));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .b
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.err("short \\u escape"))?;
        let v =
            u32::from_str_radix(std::str::from_utf8(h).unwrap_or("x"), 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or_else(|| self.err("unterminated string"))?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| self.err("invalid UTF-8")),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or_else(|| self.err("bad escape"))?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi) && self.b[self.i..].starts_with(b"\\u") {
                                self.i += 2;
                                let lo = self.hex4()?;
                                0x10000 + ((hi - 0xd800) << 10) + (lo.wrapping_sub(0xdc00) & 0x3ff)
                            } else {
                                hi
                            };
                            char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER)
                        }
                        _ => return Err(self.err("bad escape")),
                    };
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                c => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested() {
        let v = parse(r#" {"a": [1, -2.5e3, true, null], "b": {"c": "x\"yé😀"}} "#).unwrap();
        assert_eq!(v.get("a").unwrap().as_array().len(), 4);
        assert_eq!(
            v.get("b").and_then(|b| b.get("c")).and_then(Value::as_str),
            Some("x\"y\u{e9}\u{1f600}")
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("1 2").is_err());
    }
}
