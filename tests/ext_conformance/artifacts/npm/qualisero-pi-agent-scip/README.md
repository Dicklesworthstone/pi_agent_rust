# pi-agent-scip

[![npm version](https://img.shields.io/npm/v/@qualisero/pi-agent-scip.svg?t=1767701438904)](https://www.npmjs.com/package/@qualisero/pi-agent-scip)

Add Code Intelligence `tools` to [pi-coding-agent](https://github.com/mariozechner/pi). Provides fast, compiler-accurate navigation and code overview for **Python** and **TypeScript/JavaScript** projects using Sourcegraph's SCIP indexers under the hood.

Once installed globally and registered as a pi extension, the agent can automatically:

- Build a `.scip/index.scip` for your project (no prompts by default)
- Go to definition
- Find references
- List and search symbols
- Provide context-aware guidance for SCIP tool usage

All of this works **inside pi**, driven by the extension. You rarely need to call anything manually.

---

#### Contributors

<a href="https://github.com/zedrdave" title="@zedrdave"><img src="https://github.com/zedrdave.png" width="50" height="50" alt="@zedrdave" /></a>
<a href="https://github.com/austinm911" title="@austinm911"><img src="https://github.com/austinm911.png" width="50" height="50" alt="@austinm911" /></a>

---

## 1. Quick start

### Global Installation (Recommended)

```bash
# Install globally
npm install -g @qualisero/pi-agent-scip

# Create symlink for pi to discover
mkdir -p ~/.pi/agent/extensions
ln -s $(npm root -g)/@qualisero/pi-agent-scip/dist/index.js ~/.pi/agent/extensions/pi-agent-scip.js
```

### Project-Local Installation

```bash
# Install as dev dependency
npm install --save-dev @qualisero/pi-agent-scip

# Link to local extensions directory
mkdir -p .pi/extensions
ln -s $(pwd)/node_modules/@qualisero/pi-agent-scip/dist/index.js .pi/extensions/pi-agent-scip.js
```

### One-Time Setup with PI_INSTRUCTIONS.md

Alternatively, you can use the provided instructions:

1. **☢️ Read the local `PI_INSTRUCTIONS.md` file ☢️** to understand what will be changed in your global pi configuration.

2. **Run pi in this repo** and say:
   > Execute instructions in @PI_INSTRUCTIONS.md

After this one-time setup, every `pi` session can see and use the SCIP tools automatically.

---

## 2. Supported Languages

| Language | Indexer | Detection |
|----------|---------|-----------|
| **Python** | `@sourcegraph/scip-python` | `pyproject.toml`, `setup.py`, `requirements.txt`, or `.py` files |
| **TypeScript/JavaScript** | `@sourcegraph/scip-typescript` | `tsconfig.json`, `jsconfig.json`, `package.json` with TypeScript dep, or `.ts`/`.tsx` files |

Both indexers are shipped as npm dependencies and invoked automatically. You do **not** need to install them separately.

For **multi-language projects** (e.g., Python backend + TypeScript frontend), both languages are detected and indexed together.

---

## 3. Requirements

- Node.js **18+** (for pi and this package)
- pi-coding-agent **0.35.0+** (for extension API support)
- For Python: ideally a `pyproject.toml` (optional but recommended for better `scip-python` behavior)
- For TypeScript: ideally a `tsconfig.json` (will be inferred if missing)

---

## 4. How it works

Once `@qualisero/pi-agent-scip` is installed and linked, the extension:

1. **Detects project language** (Python, TypeScript/JavaScript, or both)
2. **Injects context** before the agent starts, guiding it to prefer SCIP tools
3. **Registers SCIP tools** that the agent can call:
   - `scip_find_definition` - Locate symbol definitions
   - `scip_find_references` - Find all symbol usages
   - `scip_list_symbols` - List symbols in a file
   - `scip_search_symbols` - Search symbols by name
   - `scip_project_tree` - Get project structure overview

The agent will automatically use these tools instead of manual text search or file scanning.

---

## 5. CLI status helper

```bash
pi-agent-scip-status
```

Run from a project root to see index presence, indexer availability, and the last log entry.

---

## 6. Workspace support

For monorepos and workspaces:

- **pnpm workspaces**: Detected via `pnpm-workspace.yaml`
- **Yarn workspaces**: Detected via `workspaces` field in `package.json`

The TypeScript indexer will automatically index all workspace packages.

---

## 7. Migration from v0.2.x

Version 0.3.0 migrates from the old hook/custom tool system to the new unified **extensions** API introduced in pi v0.35.0.

**Breaking changes:**
- Requires pi-coding-agent `>=0.35.0`
- Hook export removed - now uses unified extension API
- Installation path changed from `tools/` to `extensions/`

**Migration steps:**
1. Update pi to `>=0.35.0`
2. Update @qualisero/pi-agent-scip to `>=0.3.0`
3. Move symlink from `~/.pi/agent/tools/` to `~/.pi/agent/extensions/`
4. Remove any `--tool` or `--hook` flags from your pi commands (use `--extension` or `-e` if needed)

The extension will continue to work identically - no functional changes to the tools themselves.
