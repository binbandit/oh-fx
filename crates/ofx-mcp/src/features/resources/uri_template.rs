use crate::features::common;

const MAX_TEMPLATE_EXPRESSIONS: usize = 64;
pub(crate) const DEFAULT_TEMPLATE_MATCH_STEPS: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateMatch {
    Matches,
    NoMatch,
    WorkLimitExceeded,
}

#[derive(Debug)]
pub(crate) struct TemplateMatchBudget {
    remaining: usize,
    exhausted: bool,
}

impl TemplateMatchBudget {
    pub(crate) fn new(max_steps: usize) -> Self {
        Self {
            remaining: max_steps,
            exhausted: false,
        }
    }

    fn consume(&mut self, count: usize) -> bool {
        if count > self.remaining {
            self.remaining = 0;
            self.exhausted = true;
            return false;
        }
        self.remaining -= count;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Simple,
    Reserved,
    Fragment,
    Label,
    Path,
    PathParameter,
    Query,
    QueryContinuation,
}

#[derive(Debug)]
struct Expression<'a> {
    open: usize,
    close: usize,
    operator: Operator,
    variable: &'a [u8],
}

#[derive(Debug)]
struct ParsedTemplate<'a> {
    source: &'a [u8],
    expressions: Vec<Expression<'a>>,
}

impl ParsedTemplate<'_> {
    fn literal_start(&self, expression_index: usize) -> usize {
        expression_index
            .checked_sub(1)
            .map_or(0, |previous| self.expressions[previous].close + 1)
    }
}

pub(crate) fn is_valid_uri_template(template: &str) -> bool {
    parse_uri_template(template).is_some()
}

pub(crate) fn match_template_with_budget(
    template: &str,
    uri: &str,
    budget: &mut TemplateMatchBudget,
) -> TemplateMatch {
    let max_uri_bytes = common::Limits::default().uri_bytes;
    if template.len() > max_uri_bytes || uri.len() > max_uri_bytes {
        return TemplateMatch::NoMatch;
    }
    if !budget.consume(template.len().saturating_mul(2)) {
        return TemplateMatch::WorkLimitExceeded;
    }
    let Some(parsed) = parse_uri_template(template) else {
        return TemplateMatch::NoMatch;
    };
    let matches = match_from(&parsed, uri.as_bytes(), 0, 0, budget);
    if budget.exhausted {
        TemplateMatch::WorkLimitExceeded
    } else if matches {
        TemplateMatch::Matches
    } else {
        TemplateMatch::NoMatch
    }
}

fn parse_uri_template(template: &str) -> Option<ParsedTemplate<'_>> {
    let bytes = template.as_bytes();
    let mut expressions = Vec::new();
    let mut literal_start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'}' => return None,
            b'{' => {}
            _ => {
                index += 1;
                continue;
            }
        }
        if !expressions.is_empty() && literal_start == index {
            return None;
        }
        if !is_valid_literal(&bytes[literal_start..index])
            || expressions.len() >= MAX_TEMPLATE_EXPRESSIONS
        {
            return None;
        }
        let open = index;
        let length = bytes[open + 1..].iter().position(|byte| *byte == b'}')?;
        let close = open + 1 + length;
        let expression = &bytes[open + 1..close];
        if expression.is_empty() || expression.contains(&b'{') {
            return None;
        }
        expressions.push(parse_expression(expression, open, close)?);
        index = close + 1;
        literal_start = index;
    }
    is_valid_literal(&bytes[literal_start..]).then_some(ParsedTemplate {
        source: bytes,
        expressions,
    })
}

fn parse_expression(expression: &[u8], open: usize, close: usize) -> Option<Expression<'_>> {
    let operator = match expression[0] {
        b'+' => Operator::Reserved,
        b'#' => Operator::Fragment,
        b'.' => Operator::Label,
        b'/' => Operator::Path,
        b';' => Operator::PathParameter,
        b'?' => Operator::Query,
        b'&' => Operator::QueryContinuation,
        _ => Operator::Simple,
    };
    let variable = if operator == Operator::Simple {
        expression
    } else {
        &expression[1..]
    };
    is_valid_variable(variable).then_some(Expression {
        open,
        close,
        operator,
        variable,
    })
}

fn is_valid_variable(variable: &[u8]) -> bool {
    if variable.first().is_none_or(|byte| *byte == b'.') || variable.last() == Some(&b'.') {
        return false;
    }
    !variable.windows(2).any(|pair| pair == b"..")
        && variable
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
}

fn is_valid_literal(literal: &[u8]) -> bool {
    let mut index = 0;
    while index < literal.len() {
        let byte = literal[index];
        if byte == b'%' {
            if !is_percent_encoded(literal, index) {
                return false;
            }
            index += 3;
            continue;
        }
        if !is_unreserved(byte) && !is_reserved(byte) {
            return false;
        }
        index += 1;
    }
    true
}

fn is_percent_encoded(bytes: &[u8], index: usize) -> bool {
    index + 2 < bytes.len()
        && bytes[index + 1].is_ascii_hexdigit()
        && bytes[index + 2].is_ascii_hexdigit()
}

fn match_from(
    parsed: &ParsedTemplate<'_>,
    uri: &[u8],
    expression_index: usize,
    uri_index: usize,
    budget: &mut TemplateMatchBudget,
) -> bool {
    let literal_start = parsed.literal_start(expression_index);
    let Some(expression) = parsed.expressions.get(expression_index) else {
        return bounded_equal(&parsed.source[literal_start..], &uri[uri_index..], budget);
    };
    let literal = &parsed.source[literal_start..expression.open];
    if !bounded_starts_with(&uri[uri_index..], literal, budget) {
        return false;
    }
    let value_start = uri_index + literal.len();
    let next_literal_end = parsed
        .expressions
        .get(expression_index + 1)
        .map_or(parsed.source.len(), |next| next.open);
    let next_literal = &parsed.source[expression.close + 1..next_literal_end];
    if next_literal.is_empty() {
        return expression_index + 1 == parsed.expressions.len()
            && expression_matches(expression, &uri[value_start..], budget);
    }
    let mut search_start = value_start;
    while let Some(value_end) = find_literal(uri, next_literal, search_start, budget) {
        if expression_matches(expression, &uri[value_start..value_end], budget)
            && match_from(parsed, uri, expression_index + 1, value_end, budget)
        {
            return true;
        }
        if budget.exhausted {
            return false;
        }
        search_start = value_end + 1;
    }
    false
}

fn find_literal(
    uri: &[u8],
    literal: &[u8],
    start: usize,
    budget: &mut TemplateMatchBudget,
) -> Option<usize> {
    if literal.is_empty() || start > uri.len() {
        return None;
    }
    let mut search_start = start;
    while search_start < uri.len() {
        let Some(offset) = uri[search_start..]
            .iter()
            .position(|byte| *byte == literal[0])
        else {
            budget.consume(uri.len() - search_start);
            return None;
        };
        let index = search_start + offset;
        if !budget.consume(offset + 1) || literal.len() > uri.len() - index {
            return None;
        }
        if bounded_equal(&uri[index..index + literal.len()], literal, budget) {
            return Some(index);
        }
        if budget.exhausted {
            return None;
        }
        search_start = index + 1;
    }
    None
}

fn bounded_starts_with(value: &[u8], prefix: &[u8], budget: &mut TemplateMatchBudget) -> bool {
    prefix.len() <= value.len() && bounded_equal(&value[..prefix.len()], prefix, budget)
}

fn bounded_equal(left: &[u8], right: &[u8], budget: &mut TemplateMatchBudget) -> bool {
    left.len() == right.len() && budget.consume(left.len()) && left == right
}

fn expression_matches(
    expression: &Expression<'_>,
    value: &[u8],
    budget: &mut TemplateMatchBudget,
) -> bool {
    let prefixed = |prefix: u8, allow_reserved: bool, budget: &mut TemplateMatchBudget| {
        value.is_empty()
            || (value[0] == prefix && valid_expanded_value(&value[1..], allow_reserved, budget))
    };
    match expression.operator {
        Operator::Simple => valid_expanded_value(value, false, budget),
        Operator::Reserved => valid_expanded_value(value, true, budget),
        Operator::Fragment => prefixed(b'#', true, budget),
        Operator::Label => prefixed(b'.', false, budget),
        Operator::Path => prefixed(b'/', false, budget),
        Operator::PathParameter => {
            named_expansion_matches(b';', expression.variable, value, true, budget)
        }
        Operator::Query => named_expansion_matches(b'?', expression.variable, value, false, budget),
        Operator::QueryContinuation => {
            named_expansion_matches(b'&', expression.variable, value, false, budget)
        }
    }
}

fn named_expansion_matches(
    prefix: u8,
    variable: &[u8],
    value: &[u8],
    value_optional: bool,
    budget: &mut TemplateMatchBudget,
) -> bool {
    if value.is_empty() {
        return true;
    }
    if value[0] != prefix
        || value.len() < variable.len() + 1
        || !bounded_equal(&value[1..=variable.len()], variable, budget)
    {
        return false;
    }
    match &value[variable.len() + 1..] {
        [] => value_optional,
        [b'=', expansion @ ..] => valid_expanded_value(expansion, false, budget),
        _ => false,
    }
}

fn valid_expanded_value(
    value: &[u8],
    allow_reserved: bool,
    budget: &mut TemplateMatchBudget,
) -> bool {
    let mut index = 0;
    while index < value.len() {
        let byte = value[index];
        if byte == b'%' {
            if !budget.consume(3) || !is_percent_encoded(value, index) {
                return false;
            }
            index += 3;
            continue;
        }
        if !budget.consume(1) || !(is_unreserved(byte) || (allow_reserved && is_reserved(byte))) {
            return false;
        }
        index += 1;
    }
    true
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn is_reserved(byte: u8) -> bool {
    matches!(
        byte,
        b':' | b'/'
            | b'?'
            | b'#'
            | b'['
            | b']'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_may_resolve(template: &str, uri: &str) -> bool {
        let mut budget = TemplateMatchBudget::new(DEFAULT_TEMPLATE_MATCH_STEPS);
        match_template_with_budget(template, uri, &mut budget) == TemplateMatch::Matches
    }

    fn repeated(prefix: &str, byte: char, total: usize) -> String {
        let mut value = prefix.to_owned();
        value.extend(std::iter::repeat_n(byte, total - prefix.len()));
        value
    }

    #[test]
    fn resource_templates_match_supported_rfc_6570_expansions_conservatively() {
        for (template, uri, expected) in [
            ("db:///{table}/records/{id}", "db:///users/records/7", true),
            ("db:///{table}/records/{id}", "db:///anything", false),
            ("db:///{table}/records/{id}", "db:///users/record/7", false),
            (
                "https://example.test/{+path}",
                "https://example.test/a/b?view=full",
                true,
            ),
            (
                "https://example.test/{path}",
                "https://example.test/a/b",
                false,
            ),
            (
                "https://example.test/{path}",
                "https://example.test/a%2Fb",
                true,
            ),
            (
                "https://example.test/{path}",
                "https://example.test/a%2Gb",
                false,
            ),
            (
                "https://example.test/items{/id}",
                "https://example.test/items/42",
                true,
            ),
            (
                "https://example.test/search{?query}",
                "https://example.test/search?query=zig",
                true,
            ),
            (
                "https://example.test/search{?query}",
                "https://example.test/search?other=zig",
                false,
            ),
        ] {
            assert_eq!(
                template_may_resolve(template, uri),
                expected,
                "{template} {uri}"
            );
        }
    }

    #[test]
    fn resource_template_operators_follow_upstreams_expansion_rules() {
        for (template, uri, expected) in [
            ("x://a{#frag}", "x://a#b/c", true),
            ("x://a{#frag}", "x://a", true),
            ("x://a{#frag}", "x://ab", false),
            ("x://a{.ext}", "x://a.json", true),
            ("x://a{.ext}", "x://a.js/on", false),
            ("x://a{;p}", "x://a;p", true),
            ("x://a{;p}", "x://a;p=1", true),
            ("x://a{?q}", "x://a?q", false),
            ("x://a{?q}", "x://a", true),
            ("x://a?x=1{&more}", "x://a?x=1&more=2", true),
            ("x://a?x=1{&more}", "x://a?x=1&other=2", false),
            ("static://no/expressions", "static://no/expressions", true),
            ("static://no/expressions", "static://no/expression", false),
        ] {
            assert_eq!(
                template_may_resolve(template, uri),
                expected,
                "{template} {uri}"
            );
        }
    }

    #[test]
    fn resource_template_matching_accepts_bounded_long_terminal_expansions() {
        assert!(template_may_resolve(
            "memory://{value}",
            &repeated("memory://", 'a', 2 * 1024)
        ));
        let boundary = common::Limits::default().uri_bytes;
        assert!(template_may_resolve(
            "memory://{value}",
            &repeated("memory://", 'b', boundary)
        ));
        assert!(!template_may_resolve(
            "memory://{value}",
            &repeated("memory://", 'c', boundary + 1)
        ));
    }

    #[test]
    fn resource_template_matching_anchors_long_intervening_literals() {
        let left = "a".repeat(16 * 1024);
        let right = "b".repeat(16 * 1024);
        let template = "memory://{collection}/records/{id}";
        assert!(template_may_resolve(
            template,
            &format!("memory://{left}/records/{right}")
        ));
        assert!(!template_may_resolve(
            template,
            &format!("memory://{left}/recordsx{right}")
        ));
    }

    #[test]
    fn resource_template_matching_charges_repeated_prefix_literal_comparisons() {
        let mut template = repeated(
            "memory://{value}",
            'a',
            "memory://{value}".len() + 32 * 1024,
        );
        template.pop();
        template.push('b');
        let uri = repeated("memory://", 'a', common::Limits::default().uri_bytes);
        let mut budget = TemplateMatchBudget::new(DEFAULT_TEMPLATE_MATCH_STEPS);
        assert_eq!(
            match_template_with_budget(&template, &uri, &mut budget),
            TemplateMatch::WorkLimitExceeded
        );
        assert_eq!(budget.remaining, 0);
    }

    #[test]
    fn resource_template_matching_shares_one_work_budget_across_catalog_candidates() {
        let mut template = repeated("memory://{value}", 'a', "memory://{value}".len() + 1024);
        template.pop();
        template.push('b');
        let uri = repeated("memory://", 'a', "memory://".len() + 1800);
        let mut budget = TemplateMatchBudget::new(DEFAULT_TEMPLATE_MATCH_STEPS);
        assert_eq!(
            match_template_with_budget(&template, &uri, &mut budget),
            TemplateMatch::NoMatch
        );
        assert!(budget.remaining > 0);
        assert_eq!(
            match_template_with_budget(&template, &uri, &mut budget),
            TemplateMatch::WorkLimitExceeded
        );
    }

    #[test]
    fn resource_template_matching_rejects_adjacent_expressions_as_unsupported() {
        let uri = format!("memory://{}/42", "a".repeat(2048));
        assert!(!template_may_resolve("memory://{prefix}{/id}", &uri));
    }

    #[test]
    fn resource_template_matching_rejects_malformed_long_expansions_and_unsupported_syntax() {
        let malformed = format!("memory://{}%GG", "a".repeat(2048));
        assert!(!template_may_resolve("memory://{value}", &malformed));
        assert!(!template_may_resolve(
            "memory://{first,second}",
            "memory://one,two"
        ));
        assert!(!template_may_resolve("memory://{value*}", "memory://one"));
    }

    #[test]
    fn resource_template_syntax_rejects_malformed_and_unsupported_templates() {
        for template in [
            "db:///{table",
            "db:///table}",
            "db:///{table,id}",
            "db:///{table*}",
            "db:///{table:3}",
            "db:///{table}{/id}",
            "db:///%GG/{table}",
            "db:///{}",
            "db:///{..table}",
            "db:///{ta..ble}",
            "db:///{t{a}}",
            "db:///space {x}",
        ] {
            assert!(!is_valid_uri_template(template), "{template}");
        }
        for template in [
            "db:///{table}/{id}",
            "db:///{+path}/x{?q}",
            "db:///{#frag}/x{;p}/{&more}/y{.ext}",
            "file:///{a.b}/%2F",
            "static://no/expressions",
        ] {
            assert!(is_valid_uri_template(template), "{template}");
        }
        let many = "x://".to_owned() + &"/{v}".repeat(MAX_TEMPLATE_EXPRESSIONS);
        assert!(is_valid_uri_template(&many));
        assert!(!is_valid_uri_template(&(many + "/{w}")));
    }
}
