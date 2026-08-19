//! Criterion benchmarks for TUI rendering paths (PERF-8).
//!
//! Measures `build_conversation_content`, `view`, and message generation
//! at varying conversation sizes to catch rendering regressions.
#![allow(
    clippy::cast_lossless,
    clippy::format_collect,
    clippy::format_push_string,
    clippy::suboptimal_flops
)]

#[path = "bench_env.rs"]
mod bench_env;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::collections::HashMap;
use std::hint::black_box;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use asupersync::channel::mpsc;
use bubbles::viewport::Viewport;
use bubbletea::{Message, Model as BubbleteaModel};
use futures::stream;
use pi::agent::{Agent, AgentConfig};
use pi::config::Config;
use pi::interactive::{ConversationMessage, MessageRole, PiApp, PiMsg};
use pi::keybindings::KeyBindings;
use pi::model::{StreamEvent, Usage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::tools::ToolRegistry;

// ---------------------------------------------------------------------------
// Shared runtime (reused across benchmarks)
// ---------------------------------------------------------------------------

fn bench_runtime_handle() -> asupersync::runtime::RuntimeHandle {
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 8)
            .build()
            .expect("build asupersync runtime")
    })
    .handle()
}

// ---------------------------------------------------------------------------
// Dummy provider (no network, instant return)
// ---------------------------------------------------------------------------

struct DummyProvider;

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait::async_trait]
impl Provider for DummyProvider {
    fn name(&self) -> &str {
        "dummy"
    }

    fn api(&self) -> &str {
        "dummy"
    }

    fn model_id(&self) -> &str {
        "dummy-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        Ok(Box::pin(stream::empty()))
    }
}

fn dummy_model_entry() -> ModelEntry {
    let model = Model {
        id: "dummy-model".to_string(),
        name: "Dummy Model".to_string(),
        api: "dummy-api".to_string(),
        provider: "dummy".to_string(),
        base_url: "https://example.invalid".to_string(),
        reasoning: false,
        input: vec![InputType::Text],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: 4096,
        max_tokens: 1024,
        headers: HashMap::new(),
    };

    ModelEntry {
        model,
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

// ---------------------------------------------------------------------------
// App factory
// ---------------------------------------------------------------------------

fn create_bench_app() -> PiApp {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let config = Config::default();
    let tools = ToolRegistry::new(&[], &cwd, Some(&config));
    let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
    let agent = Agent::new(provider, tools, AgentConfig::default());
    let session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
    let resources = ResourceLoader::empty(config.enable_skill_commands());
    let resource_cli = ResourceCliOptions {
        no_skills: false,
        no_prompt_templates: false,
        no_extensions: false,
        no_themes: false,
        skill_paths: Vec::new(),
        prompt_paths: Vec::new(),
        extension_paths: Vec::new(),
        theme_paths: Vec::new(),
    };
    let model_entry = dummy_model_entry();
    let model_scope = vec![model_entry.clone()];
    let available_models = vec![model_entry.clone()];
    let (event_tx, _event_rx) = mpsc::channel(1024);

    // We need to keep tmp alive so the path stays valid.
    // Leak the TempDir so the directory persists for the benchmark iteration.
    let _keep = Box::leak(Box::new(tmp));

    let mut app = PiApp::new(
        agent,
        session,
        config,
        resources,
        resource_cli,
        cwd,
        model_entry,
        model_scope,
        available_models,
        None,
        Vec::new(),
        event_tx,
        bench_runtime_handle(),
        true,
        false,
        None,
        Some(KeyBindings::new()),
        Vec::new(),
        Usage::default(),
        None,
    );
    app.set_terminal_size(120, 40);
    app
}

// ---------------------------------------------------------------------------
// Conversation generators
// ---------------------------------------------------------------------------

/// Generate a realistic conversation with `n` messages cycling through roles.
fn generate_conversation(n: usize) -> Vec<ConversationMessage> {
    let mut msgs = Vec::with_capacity(n);
    for i in 0..n {
        let (role, content) = match i % 4 {
            0 => (
                MessageRole::User,
                format!("Can you explain how the module system works in message #{i}?"),
            ),
            1 => (
                MessageRole::Assistant,
                format!(
                    "Sure! Here's a detailed explanation for iteration {i}.\n\n\
                     ## Module System\n\n\
                     The module system provides **hierarchical namespacing** with the following features:\n\n\
                     - `mod` declarations create sub-modules\n\
                     - `pub` controls visibility across module boundaries\n\
                     - `use` statements bring items into scope\n\n\
                     ```rust\n\
                     pub mod utils {{\n\
                         pub fn helper() -> String {{\n\
                             \"helper output\".to_string()\n\
                         }}\n\
                     }}\n\
                     ```\n\n\
                     This approach ensures encapsulation while allowing flexible re-exports."
                ),
            ),
            2 => (
                MessageRole::Tool,
                format!(
                    "tool_use: read_file\npath: src/module_{i}.rs\n\n\
                     // File content ({} lines)\n\
                     use std::collections::HashMap;\n\
                     \n\
                     pub struct Registry {{\n\
                         entries: HashMap<String, Entry>,\n\
                     }}\n\
                     \n\
                     impl Registry {{\n\
                         pub fn new() -> Self {{ Self {{ entries: HashMap::new() }} }}\n\
                         pub fn insert(&mut self, key: String, val: Entry) {{ self.entries.insert(key, val); }}\n\
                     }}",
                    10 + i
                ),
            ),
            _ => (
                MessageRole::System,
                format!(
                    "System note #{i}: context window at {}% capacity.",
                    20 + (i % 60)
                ),
            ),
        };
        msgs.push(ConversationMessage {
            role,
            content,
            thinking: if i % 7 == 0 {
                Some(format!("Thinking about step {i}..."))
            } else {
                None
            },
            collapsed: false,
        });
    }
    msgs
}

fn load_conversation(app: &mut PiApp, messages: Vec<ConversationMessage>) {
    let _ = BubbleteaModel::update(
        app,
        Message::new(PiMsg::ConversationReset {
            messages,
            usage: Usage::default(),
            status: None,
        }),
    );
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_build_conversation_content(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_conversation_content");

    for &n in &[10, 50, 100, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let mut app = create_bench_app();
            load_conversation(&mut app, generate_conversation(n));
            b.iter(|| {
                let output = app.build_conversation_content();
                black_box(&output);
            });
        });
    }
    group.finish();
}

fn bench_view(c: &mut Criterion) {
    let mut group = c.benchmark_group("view");

    for &n in &[0, 10, 50, 200] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let mut app = create_bench_app();
            if n > 0 {
                load_conversation(&mut app, generate_conversation(n));
            }
            b.iter(|| {
                let output = BubbleteaModel::view(&app);
                black_box(&output);
            });
        });
    }
    group.finish();
}

fn bench_message_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_generation");

    for &n in &[10, 100, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let msgs = generate_conversation(n);
                black_box(&msgs);
            });
        });
    }
    group.finish();
}

fn bench_viewport_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("viewport_operations");

    // Generate large content for viewport operations.
    let large_content: String = (0..10_000)
        .map(|i| format!("Line {i}: The quick brown fox jumps over the lazy dog.\n"))
        .collect();

    group.bench_function("set_content_5000_lines", |b| {
        let mut viewport = Viewport::new(120, 40);
        let content: String = large_content
            .lines()
            .take(5000)
            .collect::<Vec<_>>()
            .join("\n");
        b.iter(|| {
            viewport.set_content(&content);
            black_box(viewport.y_offset());
        });
    });

    group.bench_function("page_up_10000_lines", |b| {
        let mut viewport = Viewport::new(120, 40);
        viewport.set_content(&large_content);
        viewport.goto_bottom();
        b.iter(|| {
            viewport.page_up();
            black_box(viewport.y_offset());
        });
    });

    group.bench_function("page_down_10000_lines", |b| {
        let mut viewport = Viewport::new(120, 40);
        viewport.set_content(&large_content);
        b.iter(|| {
            viewport.page_down();
            black_box(viewport.y_offset());
        });
    });

    group.bench_function("goto_bottom_10000_lines", |b| {
        let mut viewport = Viewport::new(120, 40);
        viewport.set_content(&large_content);
        b.iter(|| {
            viewport.goto_bottom();
            black_box(viewport.y_offset());
        });
    });

    group.finish();
}

fn bench_markdown_rendering(c: &mut Criterion) {
    use glamour::{Renderer as MarkdownRenderer, StyleConfig as GlamourStyleConfig};

    let mut group = c.benchmark_group("markdown_rendering");

    let short_msg = "Hello, **world**! This is a `short` message with _emphasis_.";

    let long_msg = {
        let mut s = String::with_capacity(10_000);
        s.push_str("# Detailed Technical Overview\n\n");
        s.push_str(
            "This document covers the **core architecture** and implementation details.\n\n",
        );
        s.push_str("## Code Example\n\n");
        s.push_str("```rust\nfn main() {\n    let data: Vec<u32> = (0..1000).collect();\n");
        s.push_str(
            "    let sum: u32 = data.iter().sum();\n    println!(\"Sum: {sum}\");\n}\n```\n\n",
        );
        s.push_str("## Key Features\n\n");
        for i in 0..20 {
            s.push_str(&format!(
                "- **Feature {i}**: Provides comprehensive support for operation #{i}\n"
            ));
        }
        s.push_str("\n## Performance Table\n\n");
        s.push_str("| Metric | Before | After | Change |\n");
        s.push_str("|--------|--------|-------|--------|\n");
        for i in 0..10 {
            s.push_str(&format!(
                "| Benchmark {i} | {:.1}ms | {:.1}ms | -{:.0}% |\n",
                10.0 + i as f64,
                5.0 + i as f64 * 0.5,
                50.0 - i as f64 * 2.0
            ));
        }
        s.push_str("\n> **Note**: All measurements taken on reference hardware.\n");
        s
    };

    let style = GlamourStyleConfig::default();

    group.bench_function("short_100_chars", |b| {
        b.iter(|| {
            let rendered = MarkdownRenderer::new()
                .with_style_config(style.clone())
                .with_word_wrap(80)
                .render(short_msg);
            black_box(&rendered);
        });
    });

    group.bench_function("long_10kb_with_code_and_tables", |b| {
        b.iter(|| {
            let rendered = MarkdownRenderer::new()
                .with_style_config(style.clone())
                .with_word_wrap(80)
                .render(&long_msg);
            black_box(&rendered);
        });
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = bench_env::criterion_config();
    targets =
        bench_build_conversation_content,
        bench_view,
        bench_message_generation,
        bench_viewport_operations,
        bench_markdown_rendering,
}
criterion_main!(benches);
