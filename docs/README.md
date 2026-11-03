# Pi documentation

Start here to use Pi, configure the agent working on your project, or contribute
to Pi itself. The guides below explain where to type commands and which features
require a particular interface or build.

## Using Pi

| I want to… | Start here |
| --- | --- |
| Install Pi and start a conversation | [Quick start](../README.md#quick-start), [installation](../README.md#installation) |
| Choose a provider or sign in | [Provider setup](providers.md), [authentication troubleshooting](provider-auth-troubleshooting.md) |
| Find commands and keyboard shortcuts | [Interactive interface](tui.md#slash-commands), [keybindings](keybindings.md) |
| Set up language servers | [LSP setup: supported servers, installation, and settings](lsp-configuration.md) |
| Keep an objective and run bounded goal prompts | [Goal availability and terminal guide](goal-mode.md#availability) |
| Change settings or models | [Settings](settings.md), [models](models.md) |
| Resume or branch a conversation | [Sessions](session.md), [conversation tree](tree.md) |
| Fix terminal or startup problems | [Terminal setup](terminal-setup.md), [troubleshooting](troubleshooting.md) |

### Match the documentation to your build

Run `pi --version` in your shell and keep the complete output, including the
commit hash. Docs on `main` describe development code; a published release can
have fewer features. Use the matching tag in GitHub's branch/tag selector, or
start from the [release notes](https://github.com/Dicklesworthstone/pi_agent_rust/releases).
For example, `/goal` was added after the published `v0.8.0` release. Some newer
source builds still report package version `0.8.0`, so the hash matters.

### Where do I type this?

| Kind | Example | Where it belongs |
| --- | --- | --- |
| Shell command | `pi --version`, `pi --help`, `pi --classic` | Your terminal, before starting Pi |
| Interactive slash command | `/help`, `/model`, `/goal status` | Pi's message editor; availability depends on the interface and build |
| Agent tool | `lsp`, `read`, `xdev` | Called by the model; ask Pi in ordinary language to use it |
| Configuration | `lsp.servers` | JSON in [settings files](settings.md#locations) |

For example, ask: “Use LSP to show diagnostics for `src/main.rs`.” The JSON
examples in the LSP tool references describe model tool arguments; they are not
slash commands or shell commands. There is no built-in `/lsp` command or
model-facing `goal` tool.

## Instructions for the agent working on your project

These guides help you supply instructions and reusable workflows to Pi's model.
They are separate from the commands you type in the application.

| Task | Guide |
| --- | --- |
| Supply project instructions in `AGENTS.md` / `CLAUDE.md` | [Context-file discovery and opt-out](../README.md#resource-resolution) |
| Give the agent reusable instructions | [Skills](skills.md) |
| Create reusable prompts you invoke | [Prompt templates](prompt-templates.md) |
| Configure delegated agents | [Subagent definitions](subagent-definitions.md), [built-in subagents](subagent-builtins.md) |
| Inspect language-server tool arguments and behavior | [LSP tool and protocol reference](lsp-configuration.md#tool-and-protocol-reference) |

## Developing Pi and integrating with it

| Audience | References |
| --- | --- |
| Contributors and coding agents changing this repository | [Repository instructions](../AGENTS.md), [development](development.md), [testing policy](testing-policy.md), [release process](releasing.md) |
| Applications embedding or controlling Pi | [SDK](sdk.md), [RPC](rpc.md), [goal SDK reference](goal-mode.md#sdk-and-protocol-reference) |
| Extension authors | [Extension architecture](extension-architecture.md), [compatibility guide](ext-compat.md), [troubleshooting](extension-troubleshooting.md) |

The repository's `AGENTS.md` governs work on Pi's own source. It is not an
end-user setup guide or a file you need to copy into your projects.

## Detailed reference and historical material

The [full reference index](../README.md#documentation-index) keeps the existing
runtime, provider, security, and operations documentation available.
[Planning documents](planning/), [contracts](contracts/), and [evidence](evidence/)
serve contributors and audits. Historical compatibility and benchmark records
do not establish which features your installed binary includes.
