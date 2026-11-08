//! Filter expressions, e.g. `package(db) & test(/integration/) & !kind(bench)`.
//!
//! Predicates: `package(..)`, `binary(..)`, `kind(..)`, `test(..)`, `all()`, `none()`.
//! Matchers: `=exact`, `~substring`, `/regex/`. Without a prefix, `test()`
//! matches a substring and the others match exactly.
//! Operators: `!`/`not`, `&`/`and`, `|`/`or`, and parentheses.

use anyhow::{bail, Result};
use regex::Regex;

#[derive(Debug, Clone)]
pub enum Filter {
    All,
    None,
    Pred(Field, Matcher),
    Not(Box<Filter>),
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
}

#[derive(Debug, Clone, Copy)]
pub enum Field {
    Package,
    Binary,
    Kind,
    Test,
}

#[derive(Debug, Clone)]
pub enum Matcher {
    Exact(String),
    Contains(String),
    Regex(Regex),
}

/// What a filter is matched against.
pub struct Subject<'a> {
    pub package: &'a str,
    pub binary: &'a str,
    pub kind: &'a str,
    pub test: &'a str,
}

impl Filter {
    pub fn parse(src: &str) -> Result<Filter> {
        let mut p = Parser { s: src, pos: 0 };
        let f = p.or()?;
        p.ws();
        if p.pos != src.len() {
            bail!("unexpected `{}` at offset {}", &src[p.pos..], p.pos);
        }
        Ok(f)
    }

    pub fn matches(&self, s: &Subject) -> bool {
        match self {
            Filter::All => true,
            Filter::None => false,
            Filter::Not(f) => !f.matches(s),
            Filter::And(a, b) => a.matches(s) && b.matches(s),
            Filter::Or(a, b) => a.matches(s) || b.matches(s),
            Filter::Pred(field, m) => {
                let value = match field {
                    Field::Package => s.package,
                    Field::Binary => s.binary,
                    Field::Kind => s.kind,
                    Field::Test => s.test,
                };
                match m {
                    Matcher::Exact(x) => value == x,
                    Matcher::Contains(x) => value.contains(x.as_str()),
                    Matcher::Regex(r) => r.is_match(value),
                }
            }
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.pos..]
    }

    fn ws(&mut self) {
        let trimmed = self.rest().trim_start();
        self.pos = self.s.len() - trimmed.len();
    }

    /// Consumes `tok` (an operator, or a keyword followed by a non-word char).
    fn eat(&mut self, tok: &str) -> bool {
        self.ws();
        let rest = self.rest();
        if !rest.starts_with(tok) {
            return false;
        }
        let is_word = tok.chars().all(|c| c.is_ascii_alphabetic());
        if is_word && rest[tok.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '(') {
            return false;
        }
        self.pos += tok.len();
        true
    }

    fn or(&mut self) -> Result<Filter> {
        let mut f = self.and()?;
        while self.eat("|") || self.eat("or") {
            f = Filter::Or(Box::new(f), Box::new(self.and()?));
        }
        Ok(f)
    }

    fn and(&mut self) -> Result<Filter> {
        let mut f = self.not()?;
        while self.eat("&") || self.eat("and") {
            f = Filter::And(Box::new(f), Box::new(self.not()?));
        }
        Ok(f)
    }

    fn not(&mut self) -> Result<Filter> {
        if self.eat("!") || self.eat("not") {
            return Ok(Filter::Not(Box::new(self.not()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Filter> {
        if self.eat("(") {
            let f = self.or()?;
            if !self.eat(")") {
                bail!("missing `)` at offset {}", self.pos);
            }
            return Ok(f);
        }
        self.ws();
        let name_len = self.rest().find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(self.rest().len());
        let name = &self.rest()[..name_len];
        let field = match name {
            "package" => Some(Field::Package),
            "binary" => Some(Field::Binary),
            "kind" => Some(Field::Kind),
            "test" => Some(Field::Test),
            "all" | "none" => None,
            "" => bail!("expected a predicate at offset {}", self.pos),
            other => bail!("unknown predicate `{other}`"),
        };
        self.pos += name_len;
        if !self.rest().starts_with('(') {
            bail!("expected `(` after `{name}`");
        }
        self.pos += 1;
        let Some(field) = field else {
            if !self.eat(")") {
                bail!("`{name}()` takes no argument");
            }
            return Ok(if name == "all" { Filter::All } else { Filter::None });
        };
        let m = self.matcher(field)?;
        Ok(Filter::Pred(field, m))
    }

    fn matcher(&mut self, field: Field) -> Result<Matcher> {
        let rest = self.rest();
        if let Some(body) = rest.strip_prefix('/') {
            // Regex up to the next unescaped `/`, which must be followed by `)`.
            let mut end = None;
            let mut escaped = false;
            for (i, c) in body.char_indices() {
                match c {
                    '\\' if !escaped => escaped = true,
                    '/' if !escaped => {
                        end = Some(i);
                        break;
                    }
                    _ => escaped = false,
                }
            }
            let end = end.ok_or_else(|| anyhow::anyhow!("unterminated regex"))?;
            let re = Regex::new(&body[..end].replace("\\/", "/"))?;
            self.pos += 1 + end + 1;
            if !self.rest().starts_with(')') {
                bail!("expected `)` after regex");
            }
            self.pos += 1;
            return Ok(Matcher::Regex(re));
        }
        let end = rest.find(')').ok_or_else(|| anyhow::anyhow!("missing `)`"))?;
        let arg = rest[..end].trim();
        self.pos += end + 1;
        Ok(if let Some(x) = arg.strip_prefix('=') {
            Matcher::Exact(x.to_string())
        } else if let Some(x) = arg.strip_prefix('~') {
            Matcher::Contains(x.to_string())
        } else if matches!(field, Field::Test) {
            Matcher::Contains(arg.to_string())
        } else {
            Matcher::Exact(arg.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject<'a>(package: &'a str, test: &'a str) -> Subject<'a> {
        Subject { package, binary: package, kind: "lib", test }
    }

    fn m(expr: &str, s: &Subject) -> bool {
        Filter::parse(expr).unwrap().matches(s)
    }

    #[test]
    fn predicates_and_operators() {
        let s = subject("myapp", "backup::tests::creates_archive");
        assert!(m("package(myapp)", &s));
        assert!(!m("package(myap)", &s));
        assert!(m("package(~myap)", &s));
        assert!(m("test(creates)", &s));
        assert!(m("test(=backup::tests::creates_archive)", &s));
        assert!(!m("test(=creates_archive)", &s));
        assert!(m("package(myapp) & test(=backup::tests::creates_archive)", &s));
        assert!(m("package(other) | test(/^backup::.*archive$/)", &s));
        assert!(m("!package(other) and not kind(bench)", &s));
        assert!(!m("(package(other) | all()) & none()", &s));
        assert!(m("test(/a\\/b/) | all()", &s));
    }

    #[test]
    fn errors() {
        assert!(Filter::parse("package(x").is_err());
        assert!(Filter::parse("pkg(x)").is_err());
        assert!(Filter::parse("test(/unterminated)").is_err());
        assert!(Filter::parse("all() all()").is_err());
    }
}
