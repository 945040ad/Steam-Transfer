//! Minimal Valve KeyValues (VDF / ACF) text parser. Keys are matched case-insensitively.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Value {
    Str(String),
    Obj(Obj),
}

/// Ordered map with case-insensitive lookup.
#[derive(Debug, Clone, Default)]
pub struct Obj(BTreeMap<String, (String, Value)>);

impl Obj {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(&key.to_ascii_lowercase()).map(|(_, v)| v)
    }
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.0.values().map(|(_, v)| v)
    }
    fn insert(&mut self, k: String, v: Value) {
        self.0.insert(k.to_ascii_lowercase(), (k, v));
    }
}

pub fn parse(text: &str) -> Obj {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    parse_obj(&chars, &mut i)
}

fn parse_obj(s: &[char], i: &mut usize) -> Obj {
    let mut obj = Obj::default();
    loop {
        let Some((key, quoted)) = next(s, i) else { return obj };
        if !quoted && key == "}" {
            return obj;
        }
        let Some((val, vq)) = next(s, i) else { return obj };
        if !vq && val == "{" {
            obj.insert(key, Value::Obj(parse_obj(s, i)));
        } else {
            obj.insert(key, Value::Str(val));
        }
        // skip an optional conditional like [$WIN32]
        let save = *i;
        match next(s, i) {
            Some((t, false)) if t.starts_with('[') => {}
            _ => *i = save,
        }
    }
}

fn next(s: &[char], i: &mut usize) -> Option<(String, bool)> {
    while *i < s.len() {
        if s[*i].is_whitespace() {
            *i += 1;
        } else if s[*i] == '/' && s.get(*i + 1) == Some(&'/') {
            while *i < s.len() && s[*i] != '\n' {
                *i += 1;
            }
        } else {
            break;
        }
    }
    let c = *s.get(*i)?;
    if c == '{' || c == '}' {
        *i += 1;
        return Some((c.to_string(), false));
    }
    let mut out = String::new();
    if c == '"' {
        *i += 1;
        while *i < s.len() && s[*i] != '"' {
            if s[*i] == '\\' && *i + 1 < s.len() {
                *i += 1;
                out.push(match s[*i] {
                    'n' => '\n',
                    't' => '\t',
                    x => x,
                });
            } else {
                out.push(s[*i]);
            }
            *i += 1;
        }
        *i += 1;
        return Some((out, true));
    }
    while *i < s.len() && !s[*i].is_whitespace() && !matches!(s[*i], '{' | '}' | '"') {
        out.push(s[*i]);
        *i += 1;
    }
    Some((out, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_manifest() {
        let t = "\"AppState\"\n{\n\t\"appid\"\t\t\"10\"\n\t\"name\"\t\t\"Counter \\\"Strike\\\"\"\n\t\"UserConfig\" { \"language\" \"english\" }\n}";
        let o = parse(t);
        let Some(Value::Obj(st)) = o.get("appstate") else { panic!() };
        assert!(matches!(st.get("APPID"), Some(Value::Str(s)) if s == "10"));
        assert!(matches!(st.get("name"), Some(Value::Str(s)) if s == "Counter \"Strike\""));
        assert!(matches!(st.get("userconfig"), Some(Value::Obj(_))));
    }
}
