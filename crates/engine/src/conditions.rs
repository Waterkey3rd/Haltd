//! What a breakpoint does when hit: its condition, hit count and log message.

use symbols::Expr;

/// Settings from the editor. Equal settings keep the running hit count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BreakpointOptions {
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitCondition {
    Equal(u64),
    Greater(u64),
    AtLeast(u64),
    Less(u64),
    AtMost(u64),
    /// Every Nth hit.
    Multiple(u64),
}

type Builder = fn(u64) -> HitCondition;

impl HitCondition {
    /// `N` and `==N` stop on exactly that hit; also `>N`, `>=N`, `<N`, `<=N`
    /// and `%N`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let forms: [(&str, Builder); 7] = [
            ("==", Self::Equal),
            (">=", Self::AtLeast),
            ("<=", Self::AtMost),
            ("=", Self::Equal),
            (">", Self::Greater),
            ("<", Self::Less),
            ("%", Self::Multiple),
        ];
        let (build, digits): (Builder, &str) = forms
            .iter()
            .find_map(|(prefix, build)| Some((*build, text.strip_prefix(prefix)?)))
            .unwrap_or((Self::Equal, text));
        let count: u64 = digits
            .trim()
            .parse()
            .map_err(|_| format!("invalid hit count {text:?}"))?;
        if count == 0 && build(1) == Self::Multiple(1) {
            return Err("a hit count of %0 can never match".to_owned());
        }
        Ok(build(count))
    }

    pub fn matches(self, hits: u64) -> bool {
        match self {
            Self::Equal(count) => hits == count,
            Self::Greater(count) => hits > count,
            Self::AtLeast(count) => hits >= count,
            Self::Less(count) => hits < count,
            Self::AtMost(count) => hits <= count,
            Self::Multiple(count) => hits.is_multiple_of(count),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogPart {
    Text(String),
    Expression(String),
}

/// Split a log message on `{expression}`; `{{` and `}}` are literal braces.
pub fn parse_log_message(template: &str) -> Result<Vec<LogPart>, String> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push('}');
            }
            '{' => {
                let mut expression = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(inner) => expression.push(inner),
                        None => return Err("a '{' in the log message is not closed".to_owned()),
                    }
                }
                symbols::parse(&expression)
                    .map_err(|error| format!("{{{expression}}}: {error}"))?;
                if !text.is_empty() {
                    parts.push(LogPart::Text(std::mem::take(&mut text)));
                }
                parts.push(LogPart::Expression(expression));
            }
            '}' => return Err("a '}' in the log message has no matching '{'".to_owned()),
            other => text.push(other),
        }
    }
    if !text.is_empty() {
        parts.push(LogPart::Text(text));
    }
    Ok(parts)
}

/// Options compiled for use on every hit, plus the hits so far.
#[derive(Clone, Debug, Default)]
pub(crate) struct Behavior {
    pub options: BreakpointOptions,
    pub condition: Option<Expr>,
    pub hit: Option<HitCondition>,
    pub log: Option<Vec<LogPart>>,
    pub hits: u64,
    /// Why these options cannot be used; the breakpoint is left unplaced.
    pub error: Option<String>,
}

impl Behavior {
    pub fn new(options: BreakpointOptions) -> Self {
        let mut behavior = Self {
            options: options.clone(),
            ..Self::default()
        };
        if let Err(error) = behavior.compile(&options) {
            behavior.error = Some(error);
        }
        behavior
    }

    fn compile(&mut self, options: &BreakpointOptions) -> Result<(), String> {
        if let Some(text) = options
            .condition
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            self.condition =
                Some(symbols::parse(text).map_err(|error| format!("condition: {error}"))?);
        }
        if let Some(text) = options
            .hit_condition
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            self.hit = Some(HitCondition::parse(text)?);
        }
        if let Some(text) = options.log_message.as_deref() {
            self.log = Some(parse_log_message(text)?);
        }
        Ok(())
    }

    /// True if every hit simply stops, with nothing to evaluate.
    pub fn is_plain(&self) -> bool {
        self.condition.is_none() && self.hit.is_none() && self.log.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_counts_take_comparison_forms() {
        assert_eq!(HitCondition::parse("5"), Ok(HitCondition::Equal(5)));
        assert_eq!(HitCondition::parse("== 5"), Ok(HitCondition::Equal(5)));
        assert_eq!(HitCondition::parse(">=3"), Ok(HitCondition::AtLeast(3)));
        assert_eq!(HitCondition::parse("%2"), Ok(HitCondition::Multiple(2)));
        for bad in ["", "abc", ">", "%0", "-1"] {
            assert!(HitCondition::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hit_counts_match() {
        assert!(HitCondition::Equal(3).matches(3));
        assert!(!HitCondition::Equal(3).matches(4));
        assert!(HitCondition::Greater(3).matches(4));
        assert!(!HitCondition::Greater(3).matches(3));
        assert!(HitCondition::Multiple(3).matches(6));
        assert!(!HitCondition::Multiple(3).matches(7));
    }

    #[test]
    fn log_messages_split_on_braces() {
        assert_eq!(
            parse_log_message("x={a.b} y={{{c}}}").unwrap(),
            [
                LogPart::Text("x=".to_owned()),
                LogPart::Expression("a.b".to_owned()),
                LogPart::Text(" y={".to_owned()),
                LogPart::Expression("c".to_owned()),
                LogPart::Text("}".to_owned()),
            ]
        );
    }

    #[test]
    fn malformed_log_messages_are_rejected() {
        for bad in ["{a", "a}", "{1a}", "{}"] {
            assert!(parse_log_message(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn bad_options_are_reported_and_blank_ones_ignored() {
        let bad = Behavior::new(BreakpointOptions {
            condition: Some("a +".to_owned()),
            ..BreakpointOptions::default()
        });
        assert!(bad.error.is_some());
        let blank = Behavior::new(BreakpointOptions {
            condition: Some("  ".to_owned()),
            hit_condition: Some(String::new()),
            log_message: None,
        });
        assert!(blank.error.is_none());
        assert!(blank.is_plain());
    }
}
