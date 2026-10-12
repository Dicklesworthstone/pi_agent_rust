//! Transactional secret screening for structured outbound values.

use super::{
    SECRETS_SCHEMA, SecretVault, SecretsMode, TransformAudit, gate_outbound, obfuscate, scan,
};
use crate::error::{Error, Result};

impl SecretVault {
    /// Remove this vault's restoration capability from a text projection.
    ///
    /// Tool-free auxiliary requests must not export a disposable vault's IDs:
    /// an advisor reply can enter a different session whose same-numbered ID
    /// represents a different credential. Keep the omission visible, but make
    /// it non-restorable. Unknown IDs are not interpreted using this vault.
    pub(crate) fn redact_placeholders(&self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        let mut cursor = 0;
        for found in super::placeholder_pattern().find_iter(text) {
            output.push_str(&text[cursor..found.start()]);
            output.push_str(if self.by_placeholder.contains_key(found.as_str()) {
                super::OVERLAP_REDACTION
            } else {
                found.as_str()
            });
            cursor = found.end();
        }
        output.push_str(&text[cursor..]);
        output
    }
}

// Structured provider inputs must be screened before JSON serialization. A raw
// replacement in serialized JSON loses assignment context, mishandles escaped
// private keys, and can corrupt quotes or silently discard object members.
const MAX_SECRET_JSON_DEPTH: usize = 128;

fn secret_json_depth(depth: usize) -> Result<()> {
    if depth > MAX_SECRET_JSON_DEPTH {
        return Err(Error::validation(
            "PI_SECRET_JSON_DEPTH: structured input exceeds the secret-screening depth limit"
                .to_string(),
        ));
    }
    Ok(())
}

/// Restore tool arguments atomically before approval or execution.
///
/// Only strings and object keys are restored, exactly once. A model can emit
/// both a placeholder key and its literal counterpart, so collecting restored
/// pairs directly into a map would silently discard one of the arguments.
/// Refuse that ambiguity rather than execute a different call. The borrowed
/// input and session vault remain unchanged on every outcome.
pub(crate) fn restore_inbound_json(
    value: &serde_json::Value,
    vault: &SecretVault,
) -> Result<serde_json::Value> {
    transform_known_json_inner(
        value,
        &|text| vault.restore(text),
        false,
        "PI_SECRET_JSON_RESTORE_KEY_COLLISION: placeholder restoration would merge distinct object members; correct the tool arguments",
        0,
    )
}

/// Mask known secrets in structured tool output without serializing first.
///
/// Ordinary primitive types are preserved. A secret echoed as a number,
/// boolean or null becomes masked text, because output must not disclose the
/// credential merely to retain its type. Key collisions and excessive depth
/// refuse the projection; callers must omit the affected output on failure,
/// never fall back to the original unscreened value. No new secrets are learned.
pub(crate) fn mask_known_json(
    value: &serde_json::Value,
    vault: &SecretVault,
) -> Result<serde_json::Value> {
    mask_known_json_with_replacements(value, vault, &vault.mask_replacements())
}

pub(super) fn mask_known_json_with_replacements(
    value: &serde_json::Value,
    vault: &SecretVault,
    replacements: &super::MaskReplacements<'_>,
) -> Result<serde_json::Value> {
    transform_known_json_inner(
        value,
        &|text| vault.mask_literal_text(text, replacements),
        true,
        "PI_SECRET_JSON_MASK_KEY_COLLISION: secret masking would merge distinct object members",
        0,
    )
}

fn transform_known_json_inner(
    value: &serde_json::Value,
    transform_text: &impl Fn(&str) -> String,
    mask_primitives: bool,
    collision_error: &'static str,
    depth: usize,
) -> Result<serde_json::Value> {
    use serde_json::Value;
    secret_json_depth(depth)?;
    match value {
        Value::String(text) => Ok(Value::String(transform_text(text))),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                transform_known_json_inner(
                    item,
                    transform_text,
                    mask_primitives,
                    collision_error,
                    depth + 1,
                )
            })
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut output = serde_json::Map::new();
            for (key, item) in map {
                let key = transform_text(key);
                if output.contains_key(&key) {
                    return Err(Error::validation(collision_error));
                }
                let item = transform_known_json_inner(
                    item,
                    transform_text,
                    mask_primitives,
                    collision_error,
                    depth + 1,
                )?;
                output.insert(key, item);
            }
            Ok(Value::Object(output))
        }
        primitive => {
            if mask_primitives {
                let text = primitive.to_string();
                let protected = transform_text(&text);
                if protected != text {
                    return Ok(Value::String(protected));
                }
            }
            Ok(primitive.clone())
        }
    }
}

fn secret_block_error() -> Error {
    Error::validation(
        "PI_SECRET_BLOCK: outbound input contains a detected or remembered secret; refusing to send"
            .to_string(),
    )
}

/// Discover before replacement so a bare echo appearing before its identifying
/// `api_key` field is protected on this same structured input.
///
/// Learns detected values into `vault` and, in block mode, refuses on any
/// detection. Never rewrites, so it cannot fail on a replacement that would
/// change a JSON type or merge object members.
pub fn discover_outbound_json(
    value: &serde_json::Value,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
) -> Result<()> {
    if mode == SecretsMode::Off {
        return Ok(());
    }
    discover_json_inner(value, vault, mode, extra_patterns, 0)
}

fn discover_json_text(
    text: &str,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
) -> Result<()> {
    if mode == SecretsMode::Block {
        gate_outbound(text, mode, extra_patterns)?;
        if vault.mask(text) != text {
            return Err(secret_block_error());
        }
        return Ok(());
    }
    let _ = obfuscate(text, vault, extra_patterns);
    Ok(())
}

fn discover_json_assignment(
    key: &str,
    value: &serde_json::Value,
    vault: &mut SecretVault,
    mode: SecretsMode,
) -> Result<()> {
    let rendered;
    let text = match value {
        serde_json::Value::String(text) => text.as_str(),
        serde_json::Value::Number(number) => {
            rendered = number.to_string();
            &rendered
        }
        _ => return Ok(()),
    };
    // Apply only built-in assignment rules to this synthetic context. User
    // regexes inspect actual keys/values separately, never invented bytes.
    let prefix = format!("{key}=");
    let contextual = format!("{prefix}{text}");
    for detection in scan(&contextual, &[]) {
        if detection.start < prefix.len() {
            continue;
        }
        if mode == SecretsMode::Block {
            return Err(secret_block_error());
        }
        let _ = vault.placeholder_for(&contextual[detection.start..detection.end], detection.label);
    }
    Ok(())
}

fn discover_json_inner(
    value: &serde_json::Value,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
    depth: usize,
) -> Result<()> {
    secret_json_depth(depth)?;
    match value {
        serde_json::Value::String(text) => {
            discover_json_text(text, vault, mode, extra_patterns)?;
        }
        serde_json::Value::Array(items) => {
            for item in items {
                discover_json_inner(item, vault, mode, extra_patterns, depth + 1)?;
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                discover_json_text(key, vault, mode, extra_patterns)?;
                discover_json_assignment(key, item, vault, mode)?;
                discover_json_inner(item, vault, mode, extra_patterns, depth + 1)?;
            }
        }
        primitive => {
            discover_json_text(&primitive.to_string(), vault, mode, extra_patterns)?;
        }
    }
    Ok(())
}

fn add_secret_audit(total: &mut TransformAudit, update: TransformAudit) {
    total.detections = total.detections.saturating_add(update.detections);
    for rule in update.rules {
        if !total.rules.contains(&rule) {
            total.rules.push(rule);
        }
    }
}

fn rewrite_json_text(
    text: &str,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
    audit: &mut TransformAudit,
) -> Result<String> {
    if mode == SecretsMode::Block {
        discover_json_text(text, vault, mode, extra_patterns)?;
        return Ok(text.to_string());
    }
    let (output, update) = obfuscate(text, vault, extra_patterns);
    add_secret_audit(audit, update);
    Ok(output)
}

#[derive(Clone, Copy)]
enum PrimitivePolicy {
    RefuseReplacement,
    PreserveModelOutput,
}

fn rewrite_json_inner(
    value: &serde_json::Value,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
    audit: &mut TransformAudit,
    depth: usize,
    primitive_policy: PrimitivePolicy,
) -> Result<serde_json::Value> {
    secret_json_depth(depth)?;
    match value {
        serde_json::Value::String(text) => Ok(serde_json::Value::String(rewrite_json_text(
            text,
            vault,
            mode,
            extra_patterns,
            audit,
        )?)),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                rewrite_json_inner(
                    item,
                    vault,
                    mode,
                    extra_patterns,
                    audit,
                    depth + 1,
                    primitive_policy,
                )
            })
            .collect::<Result<Vec<_>>>()
            .map(serde_json::Value::Array),
        serde_json::Value::Object(map) => {
            let mut output = serde_json::Map::new();
            for (key, item) in map {
                let protected_key = rewrite_json_text(key, vault, mode, extra_patterns, audit)?;
                if output.contains_key(&protected_key) {
                    return Err(Error::validation(
                        "PI_SECRET_JSON_KEY_COLLISION: secret replacement would merge distinct object members"
                            .to_string(),
                    ));
                }
                let protected_value = rewrite_json_inner(
                    item,
                    vault,
                    mode,
                    extra_patterns,
                    audit,
                    depth + 1,
                    primitive_policy,
                )?;
                output.insert(protected_key, protected_value);
            }
            Ok(serde_json::Value::Object(output))
        }
        primitive => {
            if mode == SecretsMode::Obfuscate
                && matches!(primitive_policy, PrimitivePolicy::PreserveModelOutput)
            {
                // The originating model already produced these exact values.
                // Replaying a number/bool/null without changing its type must
                // not permanently wedge that model's own tool-use history.
                // Discovery still learns these values so string echoes and
                // unrelated outbound fields receive their usual protection.
                return Ok(primitive.clone());
            }
            let text = primitive.to_string();
            if rewrite_json_text(&text, vault, mode, extra_patterns, audit)? != text {
                // Turning a numeric credential into a string placeholder and
                // later guessing its original type is not a safe round-trip.
                return Err(Error::validation(
                    "PI_SECRET_JSON_PRIMITIVE: secret replacement would change a JSON primitive's type; provide the credential as a string"
                        .to_string(),
                ));
            }
            Ok(primitive.clone())
        }
    }
}

/// Screen structured content atomically before serialization.
///
/// Neither the input nor the live vault changes on a refusal. Discovery spans
/// the whole value before replacement, so an earlier bare echo is protected
/// when a later assignment identifies it as a credential. Ordinary JSON types
/// and nonsecret keys are preserved; secret strings and keys are replaced
/// structurally. This is an explicit library operation: callers must apply it
/// to their outbound structured inputs, and restore individual string values
/// rather than running text replacement over serialized JSON.
///
/// # Errors
/// Returns a named validation error for block-mode detections (including
/// remembered values), excessive depth, replacement-key collisions, or a
/// credential in a primitive that cannot be replaced without changing type.
pub fn transform_outbound_json(
    value: &serde_json::Value,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
) -> Result<(serde_json::Value, TransformAudit)> {
    transform_json(
        value,
        vault,
        mode,
        extra_patterns,
        PrimitivePolicy::RefuseReplacement,
    )
}

/// Screen unsigned assistant tool arguments returning to their original model.
///
/// The caller must verify the assistant's provider, API and model all match the
/// destination. Only obfuscate mode preserves model-authored JSON primitives;
/// strings and keys are still screened, and block mode still refuses secrets.
/// This exception must never apply to user input, tool results, tool schemas or
/// assistant history sent to another origin.
pub(crate) fn transform_assistant_replay_json(
    value: &serde_json::Value,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
) -> Result<(serde_json::Value, TransformAudit)> {
    transform_json(
        value,
        vault,
        mode,
        extra_patterns,
        PrimitivePolicy::PreserveModelOutput,
    )
}

fn transform_json(
    value: &serde_json::Value,
    vault: &mut SecretVault,
    mode: SecretsMode,
    extra_patterns: &[regex::Regex],
    primitive_policy: PrimitivePolicy,
) -> Result<(serde_json::Value, TransformAudit)> {
    let mut audit = TransformAudit {
        schema: SECRETS_SCHEMA.to_string(),
        direction: "outbound".to_string(),
        detections: 0,
        rules: Vec::new(),
    };
    if mode == SecretsMode::Off {
        return Ok((value.clone(), audit));
    }
    let mut staged = vault.clone();
    discover_outbound_json(value, &mut staged, mode, extra_patterns)?;
    let output = rewrite_json_inner(
        value,
        &mut staged,
        mode,
        extra_patterns,
        &mut audit,
        0,
        primitive_policy,
    )?;
    *vault = staged;
    Ok((output, audit))
}

#[cfg(test)]
mod structured_outbound_tests {
    use super::*;
    use serde_json::{Value, json};

    const KEY: &str = "sk-abcdefghijklmnopqrstuvwxyz012345";
    const GENERIC: &str = "r4nd0mCredentialValue123456";

    fn protect(value: &Value, vault: &mut SecretVault) -> Result<(Value, TransformAudit)> {
        transform_outbound_json(value, vault, SecretsMode::Obfuscate, &[])
    }

    fn restore_value(value: &Value, vault: &SecretVault) -> Value {
        match value {
            Value::String(text) => Value::String(vault.restore(text)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| restore_value(item, vault))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (vault.restore(key), restore_value(item, vault)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    #[test]
    fn nested_strings_and_keys_round_trip_without_changing_nonsecret_types() {
        let mut input =
            json!({"nested": [{"secret": KEY}], "count": 4, "enabled": true, "empty": null});
        input
            .as_object_mut()
            .unwrap()
            .insert(KEY.into(), json!(["ordinary", 2]));
        let original = input.clone();
        let mut vault = SecretVault::default();
        let (output, audit) = protect(&input, &mut vault).unwrap();
        assert!(!output.to_string().contains(KEY));
        assert_eq!(output["count"], 4);
        assert_eq!(output["enabled"], true);
        assert_eq!(output["empty"], Value::Null);
        assert_eq!(restore_value(&output, &vault), original);
        assert_eq!(input, original, "input is borrowed, never mutated");
        assert_eq!(audit.detections, 2);
        assert!(!serde_json::to_string(&audit).unwrap().contains(KEY));
    }

    #[test]
    fn assignment_context_masks_an_earlier_bare_echo() {
        let input = json!({"a_echo": GENERIC, "z_config": {"api_key": GENERIC}});
        let mut vault = SecretVault::default();
        let (output, audit) = protect(&input, &mut vault).unwrap();
        assert_eq!(output["a_echo"], output["z_config"]["api_key"]);
        assert_ne!(output["a_echo"], GENERIC);
        assert_eq!(restore_value(&output, &vault), input);
        assert_eq!(audit.detections, 2);
    }

    #[test]
    fn decoded_private_key_preserves_quotes_backslashes_and_newlines() {
        let pem = concat!(
            "-----BEGIN ",
            "PRIVATE KEY-----\nabc\\def\"ghi\n-----END PRIVATE KEY-----"
        );
        let input = json!({"credentials": [pem], "path": "C:\\work\\project"});
        let mut vault = SecretVault::default();
        let (output, _) = protect(&input, &mut vault).unwrap();
        assert!(!output.to_string().contains("abc"));
        let encoded = serde_json::to_string(&output).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), output);
        assert_eq!(restore_value(&output, &vault), input);
    }

    #[test]
    fn block_refuses_assignment_only_credentials_without_mutating_vault() {
        let mut vault = SecretVault::default();
        let error = transform_outbound_json(
            &json!({"api_key": GENERIC}),
            &mut vault,
            SecretsMode::Block,
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("PI_SECRET_BLOCK"));
        assert!(!error.to_string().contains(GENERIC));
        assert_eq!(vault.len(), 0);
    }

    #[test]
    fn block_refuses_previously_remembered_bare_values() {
        let mut vault = SecretVault::default();
        let _ = obfuscate(&format!("api_key={GENERIC}"), &mut vault, &[]);
        let before = vault.len();
        let error = transform_outbound_json(&json!([GENERIC]), &mut vault, SecretsMode::Block, &[])
            .unwrap_err();
        assert!(error.to_string().contains("PI_SECRET_BLOCK"));
        assert_eq!(vault.len(), before);
    }

    #[test]
    fn replacement_key_collision_is_refused_without_dropping_a_member() {
        let mut vault = SecretVault::default();
        let mut input = json!({"<pi-secret:000001>": "first"});
        input
            .as_object_mut()
            .unwrap()
            .insert(KEY.into(), json!("second"));
        let error = protect(&input, &mut vault).unwrap_err();
        assert!(error.to_string().contains("PI_SECRET_JSON_KEY_COLLISION"));
        assert_eq!(input.as_object().unwrap().len(), 2);
        assert_eq!(
            vault.len(),
            0,
            "failed transaction must not install placeholders"
        );
    }

    #[test]
    fn failed_json_screen_does_not_consume_placeholder_ids() {
        let mut vault = SecretVault::default();
        let mut input = json!({"<pi-secret:000001>": 1});
        input.as_object_mut().unwrap().insert(KEY.into(), json!(2));
        assert!(protect(&input, &mut vault).is_err());
        let (output, _) = protect(&json!({"key": KEY}), &mut vault).unwrap();
        assert_eq!(output["key"], "<pi-secret:000001>");
    }

    #[test]
    fn numeric_credential_is_refused_rather_than_changing_argument_type() {
        let input = json!({"token": 123_456_789_012_345_678_u64});
        for mode in [SecretsMode::Obfuscate, SecretsMode::Block] {
            let mut vault = SecretVault::default();
            let error = transform_outbound_json(&input, &mut vault, mode, &[]).unwrap_err();
            assert!(error.to_string().contains(if mode == SecretsMode::Block {
                "PI_SECRET_BLOCK"
            } else {
                "PI_SECRET_JSON_PRIMITIVE"
            }));
            assert_eq!(vault.len(), 0);
        }
        assert!(input["token"].is_u64());
    }

    #[test]
    fn assistant_replay_preserves_primitives_but_protects_strings_and_keys() {
        let numeric = 123_456_789_012_345_678_u64;
        let mut input = json!({
            "a_echo": numeric.to_string(),
            "flags": [true, false, null],
            "token": numeric,
            "secret": KEY,
        });
        input
            .as_object_mut()
            .unwrap()
            .insert(KEY.to_string(), json!("ordinary"));
        let original = input.clone();
        let patterns = [regex::Regex::new(r"^(?:true|false|null)$").unwrap()];
        let mut vault = SecretVault::default();

        let (output, _) =
            transform_assistant_replay_json(&input, &mut vault, SecretsMode::Obfuscate, &patterns)
                .expect("model-authored primitive replay");

        assert_eq!(output["token"].as_u64(), Some(numeric));
        assert_eq!(output["flags"], input["flags"]);
        assert_ne!(output["a_echo"], input["a_echo"]);
        assert!(!output.to_string().contains(KEY));
        assert_eq!(restore_value(&output, &vault), original);
        assert_eq!(input, original, "only the outbound projection changes");

        // The exception does not suppress discovery or block-mode policy.
        // Check each primitive separately so the numeric assignment cannot
        // hide an accidental bool/null exemption.
        for protected in [
            json!({"token": numeric}),
            json!(true),
            json!(false),
            json!(null),
        ] {
            let mut blocked_vault = SecretVault::default();
            let error = transform_assistant_replay_json(
                &protected,
                &mut blocked_vault,
                SecretsMode::Block,
                &patterns,
            )
            .expect_err("block mode still applies to every primitive");
            assert!(error.to_string().contains("PI_SECRET_BLOCK"));
            assert_eq!(blocked_vault.len(), 0);
        }
    }

    #[test]
    fn inbound_restore_preserves_json_types_and_escaped_keys_and_values() {
        let mut vault = SecretVault::default();
        let raw = "line one\n\"quoted\" C:\\keys\\value";
        let placeholder = vault.placeholder_for(raw, "user");
        let nested_secret = "literal <pi-secret:000001>";
        let nested_placeholder = vault.placeholder_for(nested_secret, "user");
        let input = json!({
            "nested": [{placeholder.clone(): placeholder.clone()}],
            "literal": nested_placeholder,
            "ordinary": [17, true, false, null],
            "unknown": "<pi-secret:ffffff>",
        });
        let original = input.clone();

        let restored = restore_inbound_json(&input, &vault).expect("unambiguous tool arguments");

        assert_eq!(restored["nested"][0][raw], raw);
        assert_eq!(restored["literal"], nested_secret);
        assert_eq!(restored["ordinary"], input["ordinary"]);
        assert_eq!(restored["unknown"], "<pi-secret:ffffff>");
        assert_eq!(input, original);
        assert_eq!(vault.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(&restored.to_string()).unwrap(),
            restored,
        );
    }

    #[test]
    fn inbound_restore_refuses_nested_key_collisions_without_losing_arguments() {
        let mut vault = SecretVault::default();
        let placeholder = vault.placeholder_for(KEY, "user");
        let input = json!({
            "safe": "unchanged",
            "nested": [{
                KEY: "literal-member",
                placeholder.clone(): "placeholder-member",
            }],
        });
        let original = input.clone();

        let error = restore_inbound_json(&input, &vault).expect_err("ambiguous object must refuse");

        assert!(
            error
                .to_string()
                .contains("PI_SECRET_JSON_RESTORE_KEY_COLLISION")
        );
        assert!(!error.to_string().contains(KEY));
        assert_eq!(input, original);
        assert_eq!(input["nested"][0].as_object().unwrap().len(), 2);
        assert_eq!(vault.restore(&placeholder), KEY);
    }

    #[test]
    fn known_output_masking_protects_scalar_primitives_and_retains_nonsecret_types() {
        let mut vault = SecretVault::default();
        let numeric = vault.placeholder_for("1234567890123456", "user");
        let boolean = vault.placeholder_for("true", "user");
        let empty = vault.placeholder_for("null", "user");
        let cases = [
            (json!(1_234_567_890_123_456_u64), json!(numeric)),
            (json!(true), json!(boolean)),
            (Value::Null, json!(empty)),
            (json!(17), json!(17)),
            (json!(false), json!(false)),
        ];

        for (input, expected) in cases {
            let output = mask_known_json(&input, &vault).expect("screen scalar output");
            assert_eq!(output, expected);
        }
        let partial_numeric = json!(12_345_678_901_234_567_u64);
        assert_eq!(
            mask_known_json(&partial_numeric, &vault).unwrap(),
            json!(format!("{numeric}7")),
            "a numeric suffix cannot expose a remembered secret prefix",
        );
        assert_eq!(vault.len(), 3);
    }

    #[test]
    fn known_output_masking_refuses_key_collisions_atomically() {
        let mut vault = SecretVault::default();
        let placeholder = vault.placeholder_for(KEY, "user");
        let input = json!({
            "nested": {
                KEY: {"credential": KEY},
                placeholder.clone(): "a different member",
            },
        });
        let original = input.clone();

        let error = mask_known_json(&input, &vault).expect_err("ambiguous output must refuse");

        assert!(
            error
                .to_string()
                .contains("PI_SECRET_JSON_MASK_KEY_COLLISION")
        );
        assert!(!error.to_string().contains(KEY));
        assert_eq!(input, original);
        assert_eq!(input["nested"].as_object().unwrap().len(), 2);
        assert_eq!(vault.len(), 1);
    }

    #[test]
    fn inbound_and_output_projections_enforce_the_same_depth_boundary() {
        let mut vault = SecretVault::default();
        let placeholder = vault.placeholder_for(KEY, "user");
        let mut arguments = json!(placeholder);
        let mut output = json!(KEY);
        for _ in 0..MAX_SECRET_JSON_DEPTH {
            arguments = Value::Array(vec![arguments]);
            output = Value::Array(vec![output]);
        }

        assert_eq!(restore_inbound_json(&arguments, &vault).unwrap(), output);
        assert_eq!(mask_known_json(&output, &vault).unwrap(), arguments);

        arguments = Value::Array(vec![arguments]);
        output = Value::Array(vec![output]);
        for error in [
            restore_inbound_json(&arguments, &vault).unwrap_err(),
            mask_known_json(&output, &vault).unwrap_err(),
        ] {
            assert!(error.to_string().contains("PI_SECRET_JSON_DEPTH"));
            assert!(!error.to_string().contains(KEY));
        }
        assert_eq!(vault.len(), 1);
    }

    #[test]
    fn off_mode_preserves_json_and_does_not_learn_secrets() {
        let input = json!({"api_key": KEY, "token": 123_456_789_012_345_678_u64});
        let mut vault = SecretVault::default();
        let (output, audit) =
            transform_outbound_json(&input, &mut vault, SecretsMode::Off, &[]).unwrap();
        assert_eq!(output, input);
        assert_eq!(audit.detections, 0);
        assert_eq!(vault.len(), 0);
    }

    #[test]
    #[allow(clippy::trivial_regex)] // the API under test takes user-supplied regexes
    fn custom_patterns_screen_unicode_keys_and_values() {
        let mut input = json!({"value": "before 🦀secret after"});
        input
            .as_object_mut()
            .unwrap()
            .insert("🦀secret".into(), json!("untouched"));
        let patterns = [regex::Regex::new("🦀secret").unwrap()];
        let mut vault = SecretVault::default();
        let (output, audit) =
            transform_outbound_json(&input, &mut vault, SecretsMode::Obfuscate, &patterns).unwrap();
        assert!(!output.to_string().contains("🦀secret"));
        assert_eq!(restore_value(&output, &vault), input);
        assert_eq!(audit.detections, 2);
    }

    #[test]
    fn excessive_json_depth_is_refused_without_vault_changes() {
        let mut input = json!(KEY);
        for _ in 0..=MAX_SECRET_JSON_DEPTH {
            input = Value::Array(vec![input]);
        }
        let mut vault = SecretVault::default();
        let error = protect(&input, &mut vault).unwrap_err();
        assert!(error.to_string().contains("PI_SECRET_JSON_DEPTH"));
        assert_eq!(vault.len(), 0);
    }

    #[test]
    #[allow(clippy::trivial_regex)] // the API under test takes user-supplied regexes
    fn custom_regexes_never_scan_synthetic_assignment_text() {
        let input = json!({"ordinary": "short"});
        let patterns = [regex::Regex::new("ordinary=short").unwrap()];
        let mut vault = SecretVault::default();
        let (output, audit) =
            transform_outbound_json(&input, &mut vault, SecretsMode::Obfuscate, &patterns).unwrap();
        assert_eq!(output, input);
        assert_eq!(audit.detections, 0);
        assert_eq!(vault.len(), 0);
    }

    #[test]
    fn auxiliary_projection_cannot_restore_a_disposable_vaults_credentials() {
        let mut auxiliary = SecretVault::default();
        let (protected, _) = obfuscate(KEY, &mut auxiliary, &[]);
        let projection = auxiliary.redact_placeholders(&protected);
        assert_eq!(projection, "<pi-secret:redacted>");
        assert_eq!(auxiliary.restore(&projection), projection);
        let mut session = SecretVault::default();
        let _ = obfuscate("sk-otherCredential0123456789", &mut session, &[]);
        assert_ne!(session.restore(&protected), KEY);
        assert_eq!(session.restore(&projection), projection);
        assert_eq!(
            auxiliary.restore(&protected),
            KEY,
            "projection is read-only"
        );
    }

    #[test]
    fn irreversible_projection_preserves_unknown_ids_and_is_idempotent() {
        let mut vault = SecretVault::default();
        let (protected, _) = obfuscate(KEY, &mut vault, &[]);
        let text = format!("α {protected} ω <pi-secret:ffffff> <pi-secret:bad>");
        let output = vault.redact_placeholders(&text);
        assert_eq!(
            output,
            "α <pi-secret:redacted> ω <pi-secret:ffffff> <pi-secret:bad>"
        );
        assert_eq!(vault.redact_placeholders(&output), output);
        assert!(!output.contains(KEY));
    }
}
