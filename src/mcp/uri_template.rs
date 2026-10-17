//! Bounded RFC 6570 URI templates for MCP resource reads.
//!
//! This is expansion, not URI resolution: never normalize paths, open files,
//! follow links, or fetch a URI locally. All I/O stays on the original MCP
//! manager's trust-, owner-, and transport-generation-checked read path.
//!
//! All RFC operators, scalar prefixes, and list/map explode modifiers are
//! supported. Values are strings, lists, maps, or explicit nulls; composite
//! members must be strings or nulls. Missing variables remain an error rather
//! than silently selecting a different resource. Use null to omit a value.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

const MAX_TEMPLATE_BYTES: usize = 16 * 1024;
const MAX_URI_BYTES: usize = 16 * 1024;
const MAX_VARIABLES: usize = 128;
const MAX_VARIABLE_BYTES: usize = 64 * 1024;
const MAX_VARIABLE_MEMBERS: usize = 1024;
const MAX_EXPANSIONS: usize = 1024;

fn invalid(reason: &str) -> Error {
    Error::tool(
        "mcp",
        format!("[MCP_TEMPLATE_INVALID] {reason}; resource read was not sent"),
    )
}

fn limit() -> Error {
    invalid("resource template exceeds its input, expansion, or URI limit")
}

fn charge(total: &mut usize, amount: usize, maximum: usize) -> Result<()> {
    *total = total
        .checked_add(amount)
        .filter(|value| *value <= maximum)
        .ok_or_else(limit)?;
    Ok(())
}

fn validate_member(value: &Value, bytes: &mut usize) -> Result<()> {
    match value {
        Value::String(text) => charge(bytes, text.len(), MAX_VARIABLE_BYTES),
        Value::Null => Ok(()),
        _ => Err(invalid(
            "template values must be strings, nulls, or flat lists/maps of strings or nulls",
        )),
    }
}

fn validate_variables(variables: &Map<String, Value>) -> Result<()> {
    let mut bytes = 0usize;
    let mut members = 0usize;
    for (name, value) in variables {
        if !valid_name(name) {
            return Err(invalid("variable names must use RFC 6570 variable syntax"));
        }
        charge(&mut bytes, name.len(), MAX_VARIABLE_BYTES)?;
        match value {
            Value::Array(values) => {
                charge(&mut members, values.len(), MAX_VARIABLE_MEMBERS)?;
                for value in values {
                    validate_member(value, &mut bytes)?;
                }
            }
            Value::Object(values) => {
                charge(&mut members, values.len(), MAX_VARIABLE_MEMBERS)?;
                for (key, value) in values {
                    // Map keys are data, not RFC variable identifiers. Count
                    // even keys whose values are deliberately omitted.
                    charge(&mut bytes, key.len(), MAX_VARIABLE_BYTES)?;
                    validate_member(value, &mut bytes)?;
                }
            }
            _ => validate_member(value, &mut bytes)?,
        }
    }
    Ok(())
}

/// Expand a resource URI template without performing any I/O.
///
/// Values may be strings, flat lists/maps of strings or nulls, or null. Nulls
/// and empty composites are omitted; missing variables are errors. Lists
/// preserve order and duplicates; maps expand in lexical key order. Names are
/// case-sensitive and percent-encoded variable names are not decoded. `+` and
/// `#` retain reserved characters and valid percent triplets; other operators
/// encode values and map keys as components. Prefixes count Unicode characters
/// and are only valid on strings. The URI must have an absolute scheme.
///
/// # Errors
/// Rejects invalid syntax, missing variables, nested/non-string members, and
/// bounded-input/output violations. Diagnostics never echo URIs or values.
/// Limits: 16 KiB template and URI, 128 variables, 64 KiB combined variable
/// names/keys/values, 1,024 composite members, and 1,024 variable/member visits
/// during expansion. Input limits also apply to unused and null-valued data.
pub fn expand_resource_uri(template: &str, variables: &Map<String, Value>) -> Result<String> {
    if template.is_empty() || template.len() > MAX_TEMPLATE_BYTES || variables.len() > MAX_VARIABLES
    {
        return Err(limit());
    }
    validate_variables(variables)?;

    let mut output = Output(String::new());
    let mut remaining = template;
    let mut expansions = 0usize;
    while let Some(open) = remaining.find('{') {
        output.literal(&remaining[..open])?;
        remaining = &remaining[open + 1..];
        let close = remaining
            .find('}')
            .ok_or_else(|| invalid("unclosed template expression"))?;
        expand_expression(&remaining[..close], variables, &mut output, &mut expansions)?;
        remaining = &remaining[close + 1..];
    }
    output.literal(remaining)?;
    let uri = output.0;
    let scheme = uri.split_once(':').map_or("", |(scheme, _)| scheme);
    if scheme.is_empty()
        || !scheme.as_bytes()[0].is_ascii_alphabetic()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+.-".contains(&byte))
    {
        return Err(invalid(
            "expanded resource URI must have an absolute URI scheme",
        ));
    }
    Ok(uri)
}

impl super::McpManager {
    /// Expand a resource template and read it through this same trusted server.
    /// Expansion does not assert that the template appeared in a prior catalog,
    /// grant trust, open a local path, or fetch a URL outside the MCP transport.
    ///
    /// # Errors
    /// Returns template validation errors before connection setup; otherwise
    /// preserves all errors and cancellation semantics of `read_resource`.
    pub async fn read_resource_template(
        &self,
        server: &str,
        template: &str,
        variables: &Map<String, Value>,
    ) -> Result<Value> {
        let uri = expand_resource_uri(template, variables)?;
        self.read_resource(server, &uri).await
    }
}

#[derive(Clone, Copy)]
struct Operator {
    first: &'static str,
    separator: &'static str,
    named: bool,
    empty_equals: bool,
    reserved: bool,
}

impl Operator {
    fn parse(expression: &str) -> (Self, &str) {
        let mut operator = Self {
            first: "",
            separator: ",",
            named: false,
            empty_equals: false,
            reserved: false,
        };
        let Some(first) = expression.as_bytes().first() else {
            return (operator, expression);
        };
        match first {
            b'+' => operator.reserved = true,
            b'#' => {
                operator.first = "#";
                operator.reserved = true;
            }
            b'.' => {
                operator.first = ".";
                operator.separator = ".";
            }
            b'/' => {
                operator.first = "/";
                operator.separator = "/";
            }
            b';' => {
                operator.first = ";";
                operator.separator = ";";
                operator.named = true;
            }
            b'?' | b'&' => {
                operator.first = if *first == b'?' { "?" } else { "&" };
                operator.separator = "&";
                operator.named = true;
                operator.empty_equals = true;
            }
            _ => return (operator, expression),
        }
        (operator, &expression[1..])
    }
}

fn expand_expression(
    expression: &str,
    variables: &Map<String, Value>,
    output: &mut Output,
    expansions: &mut usize,
) -> Result<()> {
    let (operator, expression) = Operator::parse(expression);
    let mut writer = Expression {
        operator,
        output,
        emitted: false,
    };
    for specification in expression.split(',') {
        charge(expansions, 1, MAX_EXPANSIONS)?;
        let spec = variable_spec(specification)?;
        let value = variables.get(spec.name).ok_or_else(|| {
            invalid("a referenced variable is missing; supply a value or explicit null")
        })?;
        let members = match value {
            Value::Array(values) => values.len(),
            Value::Object(values) => values.len(),
            _ => 0,
        };
        // Empty and null members still cost work on every occurrence, even
        // when they produce no bytes. Bound that work before iterating/sorting.
        charge(expansions, members, MAX_EXPANSIONS)?;
        if spec.prefix.is_some() && matches!(value, Value::Array(_) | Value::Object(_)) {
            return Err(invalid("prefix modifiers are only valid on string values"));
        }
        match value {
            Value::String(value) => {
                let value = spec
                    .prefix
                    .map_or(value.as_str(), |length| &value[..prefix_end(value, length)]);
                writer.scalar(spec.name, value)?;
            }
            Value::Array(values) => writer.list(&spec, values)?,
            Value::Object(values) => writer.map(&spec, values)?,
            // All values were validated before expansion. Null is omission.
            _ => {}
        }
    }
    Ok(())
}

struct Expression<'a> {
    operator: Operator,
    output: &'a mut Output,
    emitted: bool,
}

impl Expression<'_> {
    fn start(&mut self) -> Result<()> {
        self.output.push(if self.emitted {
            self.operator.separator
        } else {
            self.operator.first
        })?;
        self.emitted = true;
        Ok(())
    }

    fn variable_name(&mut self, name: &str, empty: bool) -> Result<()> {
        if self.operator.named {
            // Variable spelling is literal, including its percent triplets.
            self.output.push(name)?;
            if !empty || self.operator.empty_equals {
                self.output.push("=")?;
            }
        }
        Ok(())
    }

    fn scalar(&mut self, name: &str, value: &str) -> Result<()> {
        self.start()?;
        self.variable_name(name, value.is_empty())?;
        self.output.encoded(value, self.operator.reserved)
    }

    fn list(&mut self, spec: &VariableSpec<'_>, values: &[Value]) -> Result<()> {
        let mut values = values.iter().filter_map(Value::as_str).peekable();
        if values.peek().is_none() {
            return Ok(());
        }
        if spec.explode {
            for value in values {
                self.scalar(spec.name, value)?;
            }
        } else {
            self.start()?;
            self.variable_name(spec.name, false)?;
            for (index, value) in values.enumerate() {
                if index != 0 {
                    self.output.push(",")?;
                }
                self.output.encoded(value, self.operator.reserved)?;
            }
        }
        Ok(())
    }

    fn map(&mut self, spec: &VariableSpec<'_>, values: &Map<String, Value>) -> Result<()> {
        let mut pairs: Vec<_> = values
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|value| (key.as_str(), value)))
            .collect();
        if pairs.is_empty() {
            return Ok(());
        }
        // Stable regardless of serde_json's preserve_order feature. Input
        // validation and the work budget bound this allocation and sort.
        pairs.sort_unstable_by(|left, right| left.0.cmp(right.0));
        if !spec.explode {
            self.start()?;
            self.variable_name(spec.name, false)?;
        }
        for (index, (key, value)) in pairs.into_iter().enumerate() {
            if spec.explode {
                self.start()?;
            } else if index != 0 {
                self.output.push(",")?;
            }
            self.output.encoded(key, self.operator.reserved)?;
            if !spec.explode {
                self.output.push(",")?;
            } else if !value.is_empty() || self.operator.empty_equals {
                // RFC 6570 section 3.2.1: an exploded empty map value emits
                // only its key, except that form-style operators retain '='.
                self.output.push("=")?;
            }
            self.output.encoded(value, self.operator.reserved)?;
        }
        Ok(())
    }
}

struct VariableSpec<'a> {
    name: &'a str,
    prefix: Option<usize>,
    explode: bool,
}

fn variable_spec(specification: &str) -> Result<VariableSpec<'_>> {
    // Explode is a no-op for a string. Explode+prefix is never legal.
    let explode = specification.ends_with('*');
    let (name, prefix) = if let Some(name) = specification.strip_suffix('*') {
        (name, None)
    } else if let Some((name, prefix)) = specification.split_once(':') {
        if prefix.is_empty()
            || prefix.len() > 4
            || prefix.starts_with('0')
            || !prefix.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid(
                "prefix lengths must be integers from 1 through 9999",
            ));
        }
        (
            name,
            Some(
                prefix
                    .parse::<usize>()
                    .map_err(|_| invalid("invalid prefix length"))?,
            ),
        )
    } else {
        (specification, None)
    };
    if !valid_name(name) {
        return Err(invalid(
            "invalid variable expression or unsupported template operator",
        ));
    }
    Ok(VariableSpec {
        name,
        prefix,
        explode,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|part| {
            if part.is_empty() {
                return false;
            }
            let mut offset = 0;
            let bytes = part.as_bytes();
            while offset < bytes.len() {
                if bytes[offset].is_ascii_alphanumeric() || bytes[offset] == b'_' {
                    offset += 1;
                } else if pct_octet(bytes, offset).is_some() {
                    offset += 3;
                } else {
                    return false;
                }
            }
            true
        })
}

fn pct_octet(bytes: &[u8], offset: usize) -> Option<u8> {
    if bytes.get(offset) != Some(&b'%') {
        return None;
    }
    let high = char::from(*bytes.get(offset + 1)?).to_digit(16)?;
    let low = char::from(*bytes.get(offset + 2)?).to_digit(16)?;
    u8::try_from(high * 16 + low).ok()
}

/// Keep a prefix from splitting a UTF-8 scalar or a percent-encoded scalar.
/// Invalid UTF-8 percent sequences retain each complete triplet as one unit.
fn prefix_end(value: &str, length: usize) -> usize {
    let bytes = value.as_bytes();
    let mut offset = 0;
    for _ in 0..length {
        if offset == bytes.len() {
            break;
        }
        if let Some(first) = pct_octet(bytes, offset) {
            let width = match first {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => 1,
            };
            let mut scalar = [first; 4];
            let complete = (1..width).all(|index| {
                pct_octet(bytes, offset + index * 3).is_some_and(|byte| {
                    scalar[index] = byte;
                    true
                })
            });
            offset += if complete && std::str::from_utf8(&scalar[..width]).is_ok() {
                width * 3
            } else {
                3
            };
        } else {
            offset += value[offset..].chars().next().map_or(0, char::len_utf8);
        }
    }
    offset
}

fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)
}

fn reserved(byte: u8) -> bool {
    b":/?#[]@!$&'()*+,;=".contains(&byte)
}

struct Output(String);

impl Output {
    fn push(&mut self, text: &str) -> Result<()> {
        if text.len() > MAX_URI_BYTES.saturating_sub(self.0.len()) {
            return Err(limit());
        }
        self.0.push_str(text);
        Ok(())
    }

    fn encoded(&mut self, text: &str, allow_reserved: bool) -> Result<()> {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let bytes = text.as_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            let byte = bytes[offset];
            if allow_reserved && pct_octet(bytes, offset).is_some() {
                self.push(&text[offset..offset + 3])?;
                offset += 3;
                continue;
            }
            if unreserved(byte) || (allow_reserved && reserved(byte)) {
                // This branch accepts ASCII only, so these are UTF-8 boundaries.
                self.push(&text[offset..=offset])?;
            } else {
                if MAX_URI_BYTES.saturating_sub(self.0.len()) < 3 {
                    return Err(limit());
                }
                self.0.push('%');
                self.0.push(char::from(HEX[usize::from(byte >> 4)]));
                self.0.push(char::from(HEX[usize::from(byte & 15)]));
            }
            offset += 1;
        }
        Ok(())
    }

    fn literal(&mut self, text: &str) -> Result<()> {
        // RFC 6570 section 2.1: reject malformed literals instead of creating
        // a plausible URI from a broken template. Unicode is percent encoded.
        let mut offset = 0;
        while offset < text.len() {
            if pct_octet(text.as_bytes(), offset).is_some() {
                offset += 3;
                continue;
            }
            let ch = text[offset..]
                .chars()
                .next()
                .ok_or_else(|| invalid("invalid literal"))?;
            let code = u32::from(ch);
            let allowed = matches!(code,
                0x21 | 0x23..=0x24 | 0x26 | 0x28..=0x3B | 0x3D | 0x3F..=0x5B
                | 0x5D | 0x5F | 0x61..=0x7A | 0x7E
                | 0xA0..=0xD7FF | 0xE000..=0xFDCF | 0xFDF0..=0xFFEF
            ) || (code >= 0x10000
                && (code & 0xFFFF) <= 0xFFFD
                && !(0xE0000..=0xE0FFF).contains(&code));
            if !allowed {
                return Err(invalid("template contains an invalid literal character"));
            }
            offset += ch.len_utf8();
        }
        self.encoded(text, true)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::literal_string_with_formatting_args)]
    use super::*;
    use serde_json::json;

    fn vars(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("test variable object"),
        }
    }

    #[test]
    fn scalar_rfc_operators_and_modifiers() {
        let variables = vars(json!({
            "var":"value", "hello":"Hello World!", "path":"/foo/bar",
            "x":"1024", "y":"768", "empty":"", "undef":null
        }));
        // RFC 6570 examples, prefixed with an opaque MCP scheme so they are
        // usable resource URIs rather than relative references.
        for (template, expected) in [
            ("{var}", "value"),
            ("{hello}", "Hello%20World%21"),
            ("{+hello}", "Hello%20World!"),
            ("{+path}/here", "/foo/bar/here"),
            ("{#path}", "#/foo/bar"),
            ("{x,hello,y}", "1024,Hello%20World%21,768"),
            ("X{.x,y}", "X.1024.768"),
            ("{/var,x}/here", "/value/1024/here"),
            ("{;x,y,empty}", ";x=1024;y=768;empty"),
            ("{?x,y,empty}", "?x=1024&y=768&empty="),
            ("?fixed=yes{&x}", "?fixed=yes&x=1024"),
            ("{var:3}", "val"),
            ("{var:30}", "value"),
            ("{var*}", "value"),
            ("{/var,undef}", "/value"),
            ("{/var,empty}", "/value/"),
            ("{#empty}", "#"),
            ("{?undef}", ""),
        ] {
            assert_eq!(
                expand_resource_uri(&format!("mcp:{template}"), &variables).unwrap(),
                format!("mcp:{expected}")
            );
        }
    }

    #[test]
    fn components_cannot_inject_query_fields_paths_or_fragments() {
        let variables = vars(json!({"id":"a/b?admin=true#fragment", "q":"a&b=c + d"}));
        assert_eq!(
            expand_resource_uri("docs://files/{id}{?q}", &variables).unwrap(),
            "docs://files/a%2Fb%3Fadmin%3Dtrue%23fragment?q=a%26b%3Dc%20%2B%20d"
        );
        assert_eq!(
            expand_resource_uri("docs:{+id}", &variables).unwrap(),
            "docs:a/b?admin=true#fragment"
        );
    }

    #[test]
    fn unicode_and_percent_encoded_prefixes_do_not_split_characters() {
        for value in ["🌍日本", "%F0%9F%8C%8Dtail"] {
            let variables = vars(json!({"word":value}));
            assert_eq!(
                expand_resource_uri("docs:{+word:1}", &variables).unwrap(),
                "docs:%F0%9F%8C%8D"
            );
        }
        let variables = vars(json!({"word":"%E6%97%A5tail"}));
        assert_eq!(
            expand_resource_uri("docs:{word:1}", &variables).unwrap(),
            "docs:%25E6%2597%25A5"
        );
        assert_eq!(
            expand_resource_uri("docs:日本/%2f", &Map::new()).unwrap(),
            "docs:%E6%97%A5%E6%9C%AC/%2f"
        );
    }

    #[test]
    fn reserved_expansion_preserves_only_valid_percent_triplets() {
        let variables = vars(json!({"v":"%2f%GG%"}));
        assert_eq!(
            expand_resource_uri("docs:{+v}", &variables).unwrap(),
            "docs:%2f%25GG%25"
        );
        assert_eq!(
            expand_resource_uri("docs:{v}", &variables).unwrap(),
            "docs:%252f%25GG%25"
        );
    }

    #[test]
    fn variable_names_are_exact_and_missing_values_are_not_silent_omissions() {
        let variables = vars(json!({"x.y":"one", "%78":"two", "x":"three", "skip":null}));
        assert_eq!(
            expand_resource_uri("docs:{?x.y,%78,x,skip}", &variables).unwrap(),
            "docs:?x.y=one&%78=two&x=three"
        );
        let error = expand_resource_uri("docs:{X}", &variables)
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing"));
        assert!(error.contains("read was not sent"));
    }

    #[test]
    fn malformed_templates_and_nonstring_values_fail_closed_without_echo() {
        for template in [
            "docs:{",
            "docs:}",
            "docs:{}",
            "docs:{?}",
            "docs:{v,}",
            "docs:{{v}}",
            "docs:{v:0}",
            "docs:{v:01}",
            "docs:{v:10000}",
            "docs:{v:2*}",
            "docs:{v**}",
            "docs:{=v}",
            "docs:{v..x}",
            "docs:{%GG}",
            "docs:bad%",
            "docs:bad space",
            "docs:bad\\path",
            "docs:\nprivate-sentinel",
            "relative/{v}",
        ] {
            let error = expand_resource_uri(template, &vars(json!({"v":"private-sentinel"})))
                .expect_err(template)
                .to_string();
            assert!(error.contains("MCP_TEMPLATE_INVALID"));
            assert!(!error.contains("private-sentinel"));
        }
        for value in [json!(1), json!(true), json!([1]), json!({"key":false})] {
            assert!(expand_resource_uri("docs:{v}", &vars(json!({"v":value}))).is_err());
        }
    }

    #[test]
    fn uri_limits_apply_after_encoding_and_repeated_expansion() {
        let exact = format!("docs:{}", "x".repeat(MAX_URI_BYTES - 5));
        assert_eq!(expand_resource_uri(&exact, &Map::new()).unwrap(), exact);
        assert!(expand_resource_uri(&(exact + "x"), &Map::new()).is_err());
        let variables = vars(json!({"v":" ".repeat(MAX_URI_BYTES / 3)}));
        assert!(expand_resource_uri("docs:{v}", &variables).is_err());
        let variables = vars(json!({"v":"x".repeat(MAX_URI_BYTES / 2)}));
        assert!(expand_resource_uri("docs:{v}{v}", &variables).is_err());
        let variables = vars(json!({"v":null}));
        assert!(
            expand_resource_uri(
                &format!("docs:{}", "{v}".repeat(MAX_EXPANSIONS + 1)),
                &variables
            )
            .is_err()
        );
    }

    #[test]
    fn input_limits_apply_even_to_unused_variables() {
        let variables = vars(json!({"unused":"x".repeat(MAX_VARIABLE_BYTES)}));
        assert!(expand_resource_uri("docs:static", &variables).is_err());
        let variables: Map<String, Value> = (0..=MAX_VARIABLES)
            .map(|index| (format!("v{index}"), Value::Null))
            .collect();
        assert!(expand_resource_uri("docs:static", &variables).is_err());
    }

    #[test]
    fn lists_support_every_operator_with_and_without_explode() {
        let variables = vars(json!({"list":["red","green","blue"]}));
        for (template, expected) in [
            ("{list}", "red,green,blue"),
            ("{list*}", "red,green,blue"),
            ("{+list}", "red,green,blue"),
            ("{+list*}", "red,green,blue"),
            ("{#list}", "#red,green,blue"),
            ("{#list*}", "#red,green,blue"),
            ("{.list}", ".red,green,blue"),
            ("{.list*}", ".red.green.blue"),
            ("{/list}", "/red,green,blue"),
            ("{/list*}", "/red/green/blue"),
            ("{;list}", ";list=red,green,blue"),
            ("{;list*}", ";list=red;list=green;list=blue"),
            ("{?list}", "?list=red,green,blue"),
            ("{?list*}", "?list=red&list=green&list=blue"),
            ("{&list}", "&list=red,green,blue"),
            ("{&list*}", "&list=red&list=green&list=blue"),
        ] {
            assert_eq!(
                expand_resource_uri(&format!("docs:{template}"), &variables).unwrap(),
                format!("docs:{expected}"),
                "{template}"
            );
        }
    }

    #[test]
    fn maps_support_every_operator_in_stable_key_order() {
        let variables = vars(json!({"keys":{"b":"/", "a":"x y"}}));
        for (template, expected) in [
            ("{keys}", "a,x%20y,b,%2F"),
            ("{keys*}", "a=x%20y,b=%2F"),
            ("{+keys}", "a,x%20y,b,/"),
            ("{+keys*}", "a=x%20y,b=/"),
            ("{#keys}", "#a,x%20y,b,/"),
            ("{#keys*}", "#a=x%20y,b=/"),
            ("{.keys}", ".a,x%20y,b,%2F"),
            ("{.keys*}", ".a=x%20y.b=%2F"),
            ("{/keys}", "/a,x%20y,b,%2F"),
            ("{/keys*}", "/a=x%20y/b=%2F"),
            ("{;keys}", ";keys=a,x%20y,b,%2F"),
            ("{;keys*}", ";a=x%20y;b=%2F"),
            ("{?keys}", "?keys=a,x%20y,b,%2F"),
            ("{?keys*}", "?a=x%20y&b=%2F"),
            ("{&keys}", "&keys=a,x%20y,b,%2F"),
            ("{&keys*}", "&a=x%20y&b=%2F"),
        ] {
            assert_eq!(
                expand_resource_uri(&format!("docs:{template}"), &variables).unwrap(),
                format!("docs:{expected}"),
                "{template}"
            );
        }
    }

    #[test]
    fn omitted_composites_do_not_emit_prefixes_or_separators() {
        let variables = vars(json!({
            "list":[], "map":{}, "nulls":[null,null], "none":{"key":null},
            "x":"tail", "mixed":[null,"one",null,"one"],
            "pairs":{"a":null,"b":"two"}
        }));
        for operator in ["", "+", "#", ".", "/", ";", "?", "&"] {
            assert_eq!(
                expand_resource_uri(
                    &format!("docs:{{{operator}list*,map*,nulls*,none*}}"),
                    &variables
                )
                .unwrap(),
                "docs:"
            );
        }
        assert_eq!(
            expand_resource_uri("docs:{?list*,mixed*,map*,pairs*,x}", &variables).unwrap(),
            "docs:?mixed=one&mixed=one&b=two&x=tail"
        );
        assert_eq!(
            expand_resource_uri("docs:{/list,nulls,mixed,map,pairs,none}", &variables).unwrap(),
            "docs:/one,one/b,two"
        );
    }

    #[test]
    fn empty_composite_members_are_distinct_from_omission() {
        let variables = vars(json!({"list":["","x",""], "keys":{"k":""}}));
        assert_eq!(
            expand_resource_uri("docs:{?list*}", &variables).unwrap(),
            "docs:?list=&list=x&list="
        );
        assert_eq!(
            expand_resource_uri("docs:{;list*}", &variables).unwrap(),
            "docs:;list;list=x;list"
        );
        assert_eq!(
            expand_resource_uri("docs:{/list*}", &variables).unwrap(),
            "docs://x/"
        );
        for (operator, expected) in [
            ("", "k"),
            ("+", "k"),
            ("#", "#k"),
            (".", ".k"),
            ("/", "/k"),
            (";", ";k"),
            ("?", "?k="),
            ("&", "&k="),
        ] {
            assert_eq!(
                expand_resource_uri(&format!("docs:{{{operator}keys*}}"), &variables).unwrap(),
                format!("docs:{expected}")
            );
        }
        let variables = vars(json!({"list":[""]}));
        assert_eq!(
            expand_resource_uri("docs:{;list}", &variables).unwrap(),
            "docs:;list="
        );
    }

    #[test]
    fn composite_prefixes_and_nested_or_typed_members_are_rejected_without_echo() {
        for value in [
            json!([]),
            json!({}),
            json!(["secret"]),
            json!({"secret":"value"}),
        ] {
            let error = expand_resource_uri("docs:{v:1}", &vars(json!({"v":value})))
                .unwrap_err()
                .to_string();
            assert!(error.contains("prefix"));
            assert!(!error.contains("secret"));
        }
        for value in [
            json!([["private-sentinel"]]),
            json!([{"private-sentinel":"value"}]),
            json!({"private-sentinel":[]}),
            json!({"private-sentinel":{}}),
            json!([true]),
            json!({"private-sentinel":1}),
        ] {
            // Validation covers unused input too; nested data cannot be hidden
            // behind an expression that happens not to reference it.
            let error = expand_resource_uri("docs:static", &vars(json!({"v":value})))
                .unwrap_err()
                .to_string();
            assert!(error.contains("MCP_TEMPLATE_INVALID"));
            assert!(!error.contains("private-sentinel"));
        }
    }

    #[test]
    fn composite_keys_and_values_cannot_inject_query_fields() {
        let variables = vars(json!({
            "keys":{"a&admin=1":"x/y#z"},
            "list":["?q=1&admin=true","日本","%2f"]
        }));
        assert_eq!(
            expand_resource_uri("docs:{?keys*,list*}", &variables).unwrap(),
            "docs:?a%26admin%3D1=x%2Fy%23z&list=%3Fq%3D1%26admin%3Dtrue&list=%E6%97%A5%E6%9C%AC&list=%252f"
        );
        let variables = vars(json!({"list":["%2f","%GG","🌍"]}));
        assert_eq!(
            expand_resource_uri("docs:{+list*}", &variables).unwrap(),
            "docs:%2f,%25GG,%F0%9F%8C%8D"
        );
    }

    #[test]
    fn composite_input_budgets_include_empty_members_and_map_keys() {
        let variables = vars(json!({"v":vec![Value::Null; MAX_VARIABLE_MEMBERS + 1]}));
        assert!(expand_resource_uri("docs:static", &variables).is_err());
        let variables = vars(json!({
            "left":vec![""; MAX_VARIABLE_MEMBERS / 2],
            "right":vec![Value::Null; MAX_VARIABLE_MEMBERS / 2 + 1]
        }));
        assert!(expand_resource_uri("docs:static", &variables).is_err());
        let mut values = Map::new();
        values.insert("x".repeat(MAX_VARIABLE_BYTES), Value::Null);
        let variables = vars(json!({"v":values}));
        assert!(expand_resource_uri("docs:static", &variables).is_err());
        let values: Map<String, Value> = (0..=MAX_VARIABLE_MEMBERS)
            .map(|index| (index.to_string(), Value::Null))
            .collect();
        assert!(expand_resource_uri("docs:static", &vars(json!({"v":values}))).is_err());
    }

    #[test]
    fn repeated_composite_expansion_has_a_work_budget_even_without_output() {
        let variables = vars(json!({"v":vec![Value::Null; 16]}));
        let allowed = MAX_EXPANSIONS / 17;
        assert_eq!(
            expand_resource_uri(&format!("docs:{}", "{v*}".repeat(allowed)), &variables).unwrap(),
            "docs:"
        );
        assert!(
            expand_resource_uri(&format!("docs:{}", "{v*}".repeat(allowed + 1)), &variables)
                .is_err()
        );
    }

    #[test]
    fn composite_output_limits_apply_to_keys_and_encoded_members() {
        let variables = vars(json!({"v":[" ".repeat(MAX_URI_BYTES / 3)]}));
        assert!(expand_resource_uri("docs:{?v*}", &variables).is_err());
        let mut values = Map::new();
        values.insert(" ".repeat(MAX_URI_BYTES / 3), Value::String(String::new()));
        assert!(expand_resource_uri("docs:{?v*}", &vars(json!({"v":values}))).is_err());
        let variables = vars(json!({"v":["x".repeat(MAX_URI_BYTES / 2)]}));
        assert!(expand_resource_uri("docs:{v}{v}", &variables).is_err());
    }
}
