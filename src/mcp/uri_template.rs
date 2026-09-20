//! Bounded RFC 6570 string-valued URI templates for MCP resource reads.
//!
//! This is expansion, not URI resolution: never normalize paths, open files,
//! follow links, or fetch a URI locally. All I/O stays on the original MCP
//! manager's trust-, owner-, and transport-generation-checked read path.
//!
//! All RFC operators and scalar prefix/explode modifiers are supported.
//! Variables are strings or explicit nulls. Unlike a generic template engine,
//! an absent variable is an error: accidentally omitting a selector must not
//! silently read a different resource. Use null to deliberately omit it.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

const MAX_TEMPLATE_BYTES: usize = 16 * 1024;
const MAX_URI_BYTES: usize = 16 * 1024;
const MAX_VARIABLES: usize = 128;
const MAX_VARIABLE_BYTES: usize = 64 * 1024;
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

/// Expand a string-valued resource URI template without performing any I/O.
///
/// Values must be strings or null; null explicitly omits an optional variable.
/// Names are case-sensitive and percent-encoded names are not decoded. `+` and
/// `#` retain reserved characters and valid percent triplets; other operators
/// encode values as components. Prefix lengths count Unicode characters.
/// The result must have an absolute URI scheme, but is otherwise opaque.
///
/// # Errors
/// Rejects invalid syntax, missing variables, composite/non-string values, and
/// bounded-input/output violations. Diagnostics never echo URIs or values.
/// Limits: 16 KiB template and expanded URI, 128 variables, 64 KiB combined
/// variable names/values, and 1,024 variable occurrences across expressions.
pub fn expand_resource_uri(template: &str, variables: &Map<String, Value>) -> Result<String> {
    if template.is_empty()
        || template.len() > MAX_TEMPLATE_BYTES
        || variables.len() > MAX_VARIABLES
    {
        return Err(limit());
    }
    let mut bytes = 0usize;
    for (name, value) in variables {
        if !valid_name(name) {
            return Err(invalid("variable names must use RFC 6570 variable syntax"));
        }
        let value_bytes = match value {
            Value::String(text) => text.len(),
            Value::Null => 0,
            _ => return Err(invalid("template variables must be strings or explicit nulls")),
        };
        bytes = bytes
            .checked_add(name.len())
            .and_then(|bytes| bytes.checked_add(value_bytes))
            .filter(|bytes| *bytes <= MAX_VARIABLE_BYTES)
            .ok_or_else(limit)?;
    }

    let mut output = Output(String::new());
    let mut remaining = template;
    let mut expansions = 0usize;
    while let Some(open) = remaining.find('{') {
        output.literal(&remaining[..open])?;
        remaining = &remaining[open + 1..];
        let close = remaining
            .find('}')
            .ok_or_else(|| invalid("unclosed template expression"))?;
        expand_expression(
            &remaining[..close],
            variables,
            &mut output,
            &mut expansions,
        )?;
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
        return Err(invalid("expanded resource URI must have an absolute URI scheme"));
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
    let mut emitted = false;
    for specification in expression.split(',') {
        *expansions += 1;
        if *expansions > MAX_EXPANSIONS {
            return Err(limit());
        }
        let (name, prefix) = variable_spec(specification)?;
        let value = variables.get(name).ok_or_else(|| {
            invalid("a referenced variable is missing; supply a string or explicit null")
        })?;
        let Value::String(value) = value else {
            // Values were validated before parsing. Null is deliberate omission.
            continue;
        };
        let value = prefix.map_or(value.as_str(), |length| {
            &value[..prefix_end(value, length)]
        });
        output.push(if emitted {
            operator.separator
        } else {
            operator.first
        })?;
        emitted = true;
        if operator.named {
            // Variable spelling is literal, including its percent triplets.
            output.push(name)?;
            if !value.is_empty() || operator.empty_equals {
                output.push("=")?;
            }
        }
        output.encoded(value, operator.reserved)?;
    }
    Ok(())
}

fn variable_spec(specification: &str) -> Result<(&str, Option<usize>)> {
    // Explode is a no-op for a string, per RFC 6570. Composite values are not
    // accepted by this API, and explode+prefix is not a legal combination.
    let (name, prefix) = if let Some(name) = specification.strip_suffix('*') {
        (name, None)
    } else if let Some((name, prefix)) = specification.split_once(':') {
        if prefix.is_empty()
            || prefix.len() > 4
            || prefix.starts_with('0')
            || !prefix.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid("prefix lengths must be integers from 1 through 9999"));
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
    Ok((name, prefix))
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
                if let Some(byte) = pct_octet(bytes, offset + index * 3) {
                    scalar[index] = byte;
                    true
                } else {
                    false
                }
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
                self.push(&text[offset..offset + 1])?;
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
            ) || (code >= 0x10000 && (code & 0xFFFF) <= 0xFFFD
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
    use super::*;
    use serde_json::json;

    fn vars(value: Value) -> Map<String, Value> {
        value.as_object().expect("test variable object").clone()
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
            ("{var}", "value"), ("{hello}", "Hello%20World%21"),
            ("{+hello}", "Hello%20World!"), ("{+path}/here", "/foo/bar/here"),
            ("{#path}", "#/foo/bar"), ("{x,hello,y}", "1024,Hello%20World%21,768"),
            ("X{.x,y}", "X.1024.768"), ("{/var,x}/here", "/value/1024/here"),
            ("{;x,y,empty}", ";x=1024;y=768;empty"),
            ("{?x,y,empty}", "?x=1024&y=768&empty="),
            ("?fixed=yes{&x}", "?fixed=yes&x=1024"), ("{var:3}", "val"),
            ("{var:30}", "value"), ("{var*}", "value"),
            ("{/var,undef}", "/value"), ("{/var,empty}", "/value/"),
            ("{#empty}", "#"), ("{?undef}", ""),
        ] {
            assert_eq!(expand_resource_uri(&format!("mcp:{template}"), &variables).unwrap(), format!("mcp:{expected}"));
        }
    }

    #[test]
    fn components_cannot_inject_query_fields_paths_or_fragments() {
        let variables = vars(json!({"id":"a/b?admin=true#fragment", "q":"a&b=c + d"}));
        assert_eq!(expand_resource_uri("docs://files/{id}{?q}", &variables).unwrap(),
            "docs://files/a%2Fb%3Fadmin%3Dtrue%23fragment?q=a%26b%3Dc%20%2B%20d");
        assert_eq!(expand_resource_uri("docs:{+id}", &variables).unwrap(), "docs:a/b?admin=true#fragment");
    }

    #[test]
    fn unicode_and_percent_encoded_prefixes_do_not_split_characters() {
        for value in ["🌍日本", "%F0%9F%8C%8Dtail"] {
            let variables = vars(json!({"word":value}));
            assert_eq!(expand_resource_uri("docs:{+word:1}", &variables).unwrap(), "docs:%F0%9F%8C%8D");
        }
        let variables = vars(json!({"word":"%E6%97%A5tail"}));
        assert_eq!(expand_resource_uri("docs:{word:1}", &variables).unwrap(), "docs:%25E6%2597%25A5");
        assert_eq!(expand_resource_uri("docs:日本/%2f", &Map::new()).unwrap(), "docs:%E6%97%A5%E6%9C%AC/%2f");
    }

    #[test]
    fn reserved_expansion_preserves_only_valid_percent_triplets() {
        let variables = vars(json!({"v":"%2f%GG%"}));
        assert_eq!(expand_resource_uri("docs:{+v}", &variables).unwrap(), "docs:%2f%25GG%25");
        assert_eq!(expand_resource_uri("docs:{v}", &variables).unwrap(), "docs:%252f%25GG%25");
    }

    #[test]
    fn variable_names_are_exact_and_missing_values_are_not_silent_omissions() {
        let variables = vars(json!({"x.y":"one", "%78":"two", "x":"three", "skip":null}));
        assert_eq!(expand_resource_uri("docs:{?x.y,%78,x,skip}", &variables).unwrap(), "docs:?x.y=one&%78=two&x=three");
        let error = expand_resource_uri("docs:{X}", &variables).unwrap_err().to_string();
        assert!(error.contains("missing"));
        assert!(error.contains("read was not sent"));
    }

    #[test]
    fn malformed_templates_and_nonstring_values_fail_closed_without_echo() {
        for template in [
            "docs:{", "docs:}", "docs:{}", "docs:{?}", "docs:{v,}", "docs:{{v}}",
            "docs:{v:0}", "docs:{v:01}", "docs:{v:10000}", "docs:{v:2*}",
            "docs:{v**}", "docs:{=v}", "docs:{v..x}", "docs:{%GG}", "docs:bad%",
            "docs:bad space", "docs:bad\\path", "docs:\nprivate-sentinel", "relative/{v}",
        ] {
            let error = expand_resource_uri(template, &vars(json!({"v":"private-sentinel"})))
                .expect_err(template).to_string();
            assert!(error.contains("MCP_TEMPLATE_INVALID"));
            assert!(!error.contains("private-sentinel"));
        }
        for value in [json!(1), json!(true), json!(["one"]), json!({"key":"value"})] {
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
        assert!(expand_resource_uri(&format!("docs:{}", "{v}".repeat(MAX_EXPANSIONS + 1)), &variables).is_err());
    }

    #[test]
    fn input_limits_apply_even_to_unused_variables() {
        let variables = vars(json!({"unused":"x".repeat(MAX_VARIABLE_BYTES)}));
        assert!(expand_resource_uri("docs:static", &variables).is_err());
        let variables: Map<String, Value> = (0..=MAX_VARIABLES)
            .map(|index| (format!("v{index}"), Value::Null)).collect();
        assert!(expand_resource_uri("docs:static", &variables).is_err());
    }
}
