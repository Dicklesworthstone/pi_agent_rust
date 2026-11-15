#![no_main]

//! Fuzz harness for ModelRegistry loader.
//!
//! Feeds arbitrary bytes as models.json content into ModelRegistry loader
//! (src/models.rs line 522) to test JSON parsing robustness for model
//! configuration files.

use libfuzzer_sys::fuzz_target;

// Define the types we need since they might not be in fuzz_exports yet
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelsConfig {
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api: Option<String>,
    pub api_key: Option<String>,
    pub models: Option<HashMap<String, ModelEntry>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub display_name: Option<String>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub reasoning: Option<bool>,
}

fn fuzz_models_config_parse(content: &str) {
    // This replicates the parsing logic from src/models.rs:522
    let _result: Result<ModelsConfig, _> = serde_json::from_str(content);

    // The function just needs to parse without panicking
    // Early exits on malformed JSON are expected and fine
}

fuzz_target!(|data: &[u8]| {
    let lossy = String::from_utf8_lossy(data);

    // Test direct parsing (main target)
    fuzz_models_config_parse(&lossy);

    // Test with common JSON corruption patterns
    let trimmed = lossy.trim();

    // Test with BOM prefix (common file corruption)
    let mut bom_prefixed = String::from("\u{feff}");
    bom_prefixed.push_str(&trimmed);
    fuzz_models_config_parse(&bom_prefixed);

    // Test with different line endings (Windows/Unix/Mac)
    if trimmed.contains('\n') {
        let windows_endings = trimmed.replace('\n', "\r\n");
        fuzz_models_config_parse(&windows_endings);

        let mac_endings = trimmed.replace('\n', "\r");
        fuzz_models_config_parse(&mac_endings);
    }

    // Test truncated versions (common corruption pattern)
    for i in [1, 8, 32, 128, 512, 1024].iter() {
        if let Some(truncated) = trimmed.get(..*i) {
            fuzz_models_config_parse(truncated);
        }
    }
});
