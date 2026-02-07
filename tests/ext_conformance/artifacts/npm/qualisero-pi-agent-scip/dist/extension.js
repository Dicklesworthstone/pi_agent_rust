import { promises as fs } from 'node:fs';
import { join } from 'node:path';
import { Type } from '@sinclair/typebox';
import { ScipIndexer } from './core/indexer.js';
import { ScipQuery, NeedsReindexError } from './core/query.js';
import { StructuredLogger } from './core/logger.js';
import { truncateHead, buildTruncationNotice, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES, formatSize, } from './core/truncate.js';
// Cache detected languages per cwd to avoid re-scanning on every agent start
const languageCache = new Map();
async function detectLanguages(cwd) {
    const cached = languageCache.get(cwd);
    if (cached)
        return cached;
    const result = { python: false, typescript: false };
    try {
        // Check for Python
        const pyproject = join(cwd, 'pyproject.toml');
        const setup = join(cwd, 'setup.py');
        const hasPyproject = await fs.access(pyproject).then(() => true).catch(() => false);
        const hasSetup = await fs.access(setup).then(() => true).catch(() => false);
        if (hasPyproject || hasSetup) {
            result.python = true;
        }
        else {
            // Fallback: shallow scan for .py files in src/ and root
            const srcDir = join(cwd, 'src');
            const srcEntries = await fs.readdir(srcDir).catch(() => []);
            if (srcEntries.some((e) => e.endsWith('.py'))) {
                result.python = true;
            }
            else {
                const rootEntries = await fs.readdir(cwd);
                if (rootEntries.some((e) => e.endsWith('.py'))) {
                    result.python = true;
                }
            }
        }
        // Check for TypeScript/JavaScript
        const tsconfig = join(cwd, 'tsconfig.json');
        const jsconfig = join(cwd, 'jsconfig.json');
        const packageJson = join(cwd, 'package.json');
        const hasTsconfig = await fs.access(tsconfig).then(() => true).catch(() => false);
        const hasJsconfig = await fs.access(jsconfig).then(() => true).catch(() => false);
        if (hasTsconfig || hasJsconfig) {
            result.typescript = true;
        }
        else {
            // Check package.json for TypeScript dependency
            try {
                const content = await fs.readFile(packageJson, 'utf-8');
                const pkg = JSON.parse(content);
                const deps = { ...pkg.dependencies, ...pkg.devDependencies };
                if (deps['typescript']) {
                    result.typescript = true;
                }
            }
            catch {
                // No package.json or invalid JSON
            }
            // Fallback: shallow scan for .ts/.tsx files
            if (!result.typescript) {
                const srcDir = join(cwd, 'src');
                const srcEntries = await fs.readdir(srcDir).catch(() => []);
                if (srcEntries.some((e) => e.endsWith('.ts') || e.endsWith('.tsx'))) {
                    result.typescript = true;
                }
                else {
                    const rootEntries = await fs.readdir(cwd);
                    if (rootEntries.some((e) => e.endsWith('.ts') || e.endsWith('.tsx'))) {
                        result.typescript = true;
                    }
                }
            }
        }
    }
    catch {
        // Ignore errors, return defaults
    }
    languageCache.set(cwd, result);
    return result;
}
// Track whether we've already injected a message for this session
let messageInjected = false;
export default function (pi) {
    // cwd is captured from the first event context
    let cwd;
    let logger;
    let indexer;
    let query;
    const ensureCwd = (ctx) => {
        if (!cwd) {
            cwd = ctx.cwd;
            logger = new StructuredLogger(cwd);
            indexer = new ScipIndexer(cwd, logger);
            query = new ScipQuery(cwd);
        }
    };
    const ensureIndex = async (reason, signal, onProgress) => {
        if (await query.indexExists())
            return;
        logger.log({ source: 'tool', action: 'index_missing', tool: reason });
        // Auto-generate the index by default to minimize interaction.
        // In TUI mode we only show progress messages; no confirmation dialog.
        onProgress?.('No SCIP index found. Generating one now...');
        await runIndexGeneration(reason, signal, onProgress);
        onProgress?.('SCIP index generation complete.');
    };
    const runIndexGeneration = async (caller, signal, onProgress) => {
        logger.log({ source: 'tool', action: 'reindex_start', tool: caller });
        try {
            await indexer.generateIndex({
                signal,
                onProgress,
                // Auto-accept bundled indexer usage to avoid extra prompts.
                confirmInstall: undefined,
            });
            logger.log({ source: 'tool', action: 'reindex_complete', tool: caller });
        }
        catch (error) {
            logger.log({
                source: 'tool',
                action: 'reindex_failed',
                tool: caller,
                level: 'error',
                message: error instanceof Error ? error.message : String(error),
            });
            throw error;
        }
        finally {
            query.clearCache();
        }
    };
    // Reset flag on session start
    pi.on('session_start', async (_event, ctx) => {
        ensureCwd(ctx);
        messageInjected = false;
    });
    // Inject guidance message before the first agent turn
    pi.on('before_agent_start', async (_event, ctx) => {
        ensureCwd(ctx);
        if (messageInjected)
            return;
        const languages = await detectLanguages(ctx.cwd);
        const hasAnyLanguage = languages.python || languages.typescript;
        if (!hasAnyLanguage)
            return;
        const languageNames = [];
        if (languages.python)
            languageNames.push('Python');
        if (languages.typescript)
            languageNames.push('TypeScript/JavaScript');
        const languageList = languageNames.join(' and ');
        messageInjected = true;
        return {
            message: {
                customType: 'pi-agent-scip-hint',
                content: `For this ${languageList} project, prefer the scip_* tools from @qualisero/pi-agent-scip for code navigation and structure: ` +
                    'use scip_find_definition, scip_find_references, scip_list_symbols, scip_search_symbols, and scip_project_tree ' +
                    'instead of ad-hoc text search or manual file scanning.',
                display: false, // Don't clutter the UI, just send to LLM
            },
        };
    });
    // Register SCIP tools
    pi.registerTool({
        name: 'scip_find_definition',
        label: 'SCIP: Find Definition',
        description: `Locate the definition of a symbol using SCIP indexes. Output is truncated to ${DEFAULT_MAX_LINES} lines or ${formatSize(DEFAULT_MAX_BYTES)} (whichever is hit first).`,
        parameters: Type.Object({
            symbol: Type.String({ description: 'Symbol to find (class, function, variable)' }),
            file: Type.Optional(Type.String({ description: 'Current file path (optional)' })),
        }),
        async execute(toolCallId, params, onUpdate, ctx, signal) {
            const toolName = 'scip_find_definition';
            ensureCwd(ctx);
            logger.log({
                source: 'tool',
                action: 'execute',
                tool: toolName,
                symbol: params.symbol,
                file: params.file,
            });
            const emitProgress = (text) => {
                onUpdate?.({
                    content: [{ type: 'text', text }],
                    details: [],
                });
            };
            await ensureIndex(toolName, signal, emitProgress);
            const runQuery = async () => {
                const results = await query.findDefinition(params.symbol, params.file);
                if (results.length === 0) {
                    logger.log({ source: 'tool', action: 'query_complete', tool: toolName, hits: 0 });
                    return {
                        content: [{ type: 'text', text: `No definition found for '${params.symbol}'` }],
                        details: [],
                    };
                }
                logger.log({
                    source: 'tool',
                    action: 'query_complete',
                    tool: toolName,
                    hits: results.length,
                });
                const lines = results.map((r) => {
                    const locationStr = r.line !== undefined && r.character !== undefined
                        ? `:${r.line + 1}:${r.character + 1}`
                        : '';
                    return `${r.file}${locationStr}`;
                });
                const output = lines.join('\n');
                const truncation = truncateHead(output);
                const finalOutput = truncation.content + buildTruncationNotice(truncation, toolName);
                return {
                    content: [{ type: 'text', text: finalOutput }],
                    details: truncation.truncated ? { results, truncation } : results,
                };
            };
            try {
                return await runQuery();
            }
            catch (error) {
                if (error instanceof NeedsReindexError) {
                    logger.log({ source: 'tool', action: 'index_stale', tool: toolName });
                    emitProgress('SCIP index is outdated. Regenerating...');
                    await runIndexGeneration(toolName, signal, emitProgress);
                    return runQuery();
                }
                throw error;
            }
        },
    });
    pi.registerTool({
        name: 'scip_find_references',
        label: 'SCIP: Find References',
        description: `Find all references to a symbol across the project. Output is truncated to ${DEFAULT_MAX_LINES} lines or ${formatSize(DEFAULT_MAX_BYTES)} (whichever is hit first).`,
        parameters: Type.Object({
            symbol: Type.String({ description: 'Symbol name to search for' }),
            limit: Type.Optional(Type.Number({ description: 'Maximum results', default: 50, minimum: 1, maximum: 500 })),
        }),
        async execute(toolCallId, params, onUpdate, ctx, signal) {
            const toolName = 'scip_find_references';
            ensureCwd(ctx);
            const limit = params.limit ?? 50;
            logger.log({
                source: 'tool',
                action: 'execute',
                tool: toolName,
                symbol: params.symbol,
                limit,
            });
            const emitProgress = (text) => {
                onUpdate?.({
                    content: [{ type: 'text', text }],
                    details: [],
                });
            };
            await ensureIndex(toolName, signal, emitProgress);
            const runQuery = async () => {
                const results = await query.findReferences(params.symbol);
                if (results.length === 0) {
                    logger.log({ source: 'tool', action: 'query_complete', tool: toolName, hits: 0 });
                    return {
                        content: [{ type: 'text', text: `No references found for '${params.symbol}'` }],
                        details: [],
                    };
                }
                logger.log({
                    source: 'tool',
                    action: 'query_complete',
                    tool: toolName,
                    hits: results.length,
                });
                const lines = results.map((r) => {
                    const locationStr = r.line !== undefined && r.character !== undefined
                        ? `:${r.line + 1}:${r.character + 1}`
                        : '';
                    return `${r.file}${locationStr}`;
                });
                const output = lines.join('\n');
                const truncation = truncateHead(output);
                const finalOutput = truncation.content + buildTruncationNotice(truncation, toolName);
                return {
                    content: [{ type: 'text', text: finalOutput }],
                    details: truncation.truncated ? { results, truncation } : results,
                };
            };
            try {
                return await runQuery();
            }
            catch (error) {
                if (error instanceof NeedsReindexError) {
                    logger.log({ source: 'tool', action: 'index_stale', tool: toolName });
                    emitProgress('SCIP index is outdated. Regenerating...');
                    await runIndexGeneration(toolName, signal, emitProgress);
                    return runQuery();
                }
                throw error;
            }
        },
    });
    pi.registerTool({
        name: 'scip_list_symbols',
        label: 'SCIP: List Symbols',
        description: `List all symbols defined in a single file. Output is truncated to ${DEFAULT_MAX_LINES} lines or ${formatSize(DEFAULT_MAX_BYTES)} (whichever is hit first).`,
        parameters: Type.Object({
            file: Type.String({ description: 'Relative file path within the project' }),
        }),
        async execute(toolCallId, params, onUpdate, ctx, signal) {
            const toolName = 'scip_list_symbols';
            ensureCwd(ctx);
            logger.log({
                source: 'tool',
                action: 'execute',
                tool: toolName,
                file: params.file,
            });
            const emitProgress = (text) => {
                onUpdate?.({
                    content: [{ type: 'text', text }],
                    details: [],
                });
            };
            await ensureIndex(toolName, signal, emitProgress);
            const runQuery = async () => {
                const symbols = await query.listSymbols(params.file);
                if (symbols.length === 0) {
                    logger.log({ source: 'tool', action: 'query_complete', tool: toolName, symbols: 0 });
                    return {
                        content: [{ type: 'text', text: `No symbols found in '${params.file}'` }],
                        details: [],
                    };
                }
                logger.log({
                    source: 'tool',
                    action: 'query_complete',
                    tool: toolName,
                    symbols: symbols.length,
                });
                const lines = symbols.map((s) => `${s.kind}: ${s.name}`);
                const output = lines.join('\n');
                const truncation = truncateHead(output);
                const finalOutput = truncation.content + buildTruncationNotice(truncation, toolName);
                return {
                    content: [{ type: 'text', text: finalOutput }],
                    details: truncation.truncated ? { symbols, truncation } : symbols,
                };
            };
            try {
                return await runQuery();
            }
            catch (error) {
                if (error instanceof NeedsReindexError) {
                    logger.log({ source: 'tool', action: 'index_stale', tool: toolName });
                    emitProgress('SCIP index is outdated. Regenerating...');
                    await runIndexGeneration(toolName, signal, emitProgress);
                    return runQuery();
                }
                throw error;
            }
        },
    });
    pi.registerTool({
        name: 'scip_search_symbols',
        label: 'SCIP: Search Symbols',
        description: `Search for symbols by (partial) name across the project. Output is truncated to ${DEFAULT_MAX_LINES} lines or ${formatSize(DEFAULT_MAX_BYTES)} (whichever is hit first).`,
        parameters: Type.Object({
            query: Type.String({ description: 'Substring to match against symbol names' }),
            limit: Type.Optional(Type.Number({ description: 'Maximum results', default: 20, minimum: 1, maximum: 100 })),
        }),
        async execute(toolCallId, params, onUpdate, ctx, signal) {
            const toolName = 'scip_search_symbols';
            ensureCwd(ctx);
            const limit = params.limit ?? 20;
            logger.log({
                source: 'tool',
                action: 'execute',
                tool: toolName,
                query: params.query,
                limit,
            });
            const emitProgress = (text) => {
                onUpdate?.({
                    content: [{ type: 'text', text }],
                    details: [],
                });
            };
            await ensureIndex(toolName, signal, emitProgress);
            const runQuery = async () => {
                const results = await query.searchSymbols(params.query);
                if (results.length === 0) {
                    logger.log({ source: 'tool', action: 'query_complete', tool: toolName, hits: 0 });
                    return {
                        content: [{ type: 'text', text: `No symbols matching '${params.query}'` }],
                        details: [],
                    };
                }
                logger.log({
                    source: 'tool',
                    action: 'query_complete',
                    tool: toolName,
                    hits: results.length,
                });
                const lines = results.map((r) => {
                    const locationStr = r.line !== undefined && r.character !== undefined
                        ? `:${r.line + 1}:${r.character + 1}`
                        : '';
                    return `${r.kind}: ${r.name} (${r.file}${locationStr})`;
                });
                const output = lines.join('\n');
                const truncation = truncateHead(output);
                const finalOutput = truncation.content + buildTruncationNotice(truncation, toolName);
                return {
                    content: [{ type: 'text', text: finalOutput }],
                    details: truncation.truncated ? { results, truncation } : results,
                };
            };
            try {
                return await runQuery();
            }
            catch (error) {
                if (error instanceof NeedsReindexError) {
                    logger.log({ source: 'tool', action: 'index_stale', tool: toolName });
                    emitProgress('SCIP index is outdated. Regenerating...');
                    await runIndexGeneration(toolName, signal, emitProgress);
                    return runQuery();
                }
                throw error;
            }
        },
    });
    function renderTree(tree, depth) {
        const lines = [];
        const printNode = (node, indent, prefix) => {
            if (indent > depth)
                return;
            const kindLabel = node.kind === 'Module' ? '📦' : node.kind === 'Class' ? '🏛️' : '⚙️';
            const locationStr = node.file ? ` (${node.file})` : '';
            lines.push(`${prefix}${kindLabel} ${node.name}${locationStr}`);
            if (node.children && indent < depth) {
                node.children.forEach((child, idx) => {
                    const isLast = idx === node.children.length - 1;
                    const childPrefix = prefix + (isLast ? '  └─ ' : '  ├─ ');
                    const nextPrefix = prefix + (isLast ? '     ' : '  │  ');
                    printNode(child, indent + 1, childPrefix);
                });
            }
        };
        tree.forEach((node) => {
            printNode(node, 0, '');
        });
        return lines.join('\n');
    }
    pi.registerTool({
        name: 'scip_project_tree',
        label: 'SCIP: Project Tree',
        description: `Summarize the code structure of the current project as a tree. Output is truncated to ${DEFAULT_MAX_LINES} lines or ${formatSize(DEFAULT_MAX_BYTES)} (whichever is hit first).`,
        parameters: Type.Object({
            depth: Type.Optional(Type.Number({ description: 'Maximum tree depth for text output', default: 3, minimum: 1, maximum: 10 })),
        }),
        async execute(toolCallId, params, onUpdate, ctx, signal) {
            const toolName = 'scip_project_tree';
            ensureCwd(ctx);
            const depth = params.depth ?? 3;
            logger.log({
                source: 'tool',
                action: 'execute',
                tool: toolName,
                depth,
            });
            const emitProgress = (text) => {
                onUpdate?.({
                    content: [{ type: 'text', text }],
                    details: [],
                });
            };
            await ensureIndex(toolName, signal, emitProgress);
            const runQuery = async () => {
                const tree = await query.buildProjectTree();
                logger.log({ source: 'tool', action: 'query_complete', tool: toolName, modules: tree.length });
                const rendered = renderTree(tree, depth);
                const truncation = truncateHead(rendered);
                const finalOutput = truncation.content + buildTruncationNotice(truncation, toolName);
                return {
                    content: [{ type: 'text', text: finalOutput }],
                    details: truncation.truncated ? { tree, truncation } : tree,
                };
            };
            try {
                return await runQuery();
            }
            catch (error) {
                if (error instanceof NeedsReindexError) {
                    logger.log({ source: 'tool', action: 'index_stale', tool: toolName });
                    emitProgress('SCIP index is outdated. Regenerating...');
                    await runIndexGeneration(toolName, signal, emitProgress);
                    return runQuery();
                }
                throw error;
            }
        },
    });
    pi.registerTool({
        name: 'scip_reindex',
        label: 'SCIP: Reindex',
        description: 'Regenerate the SCIP index for the project (use when code has changed significantly)',
        parameters: Type.Object({}),
        async execute(toolCallId, params, onUpdate, ctx, signal) {
            const toolName = 'scip_reindex';
            ensureCwd(ctx);
            logger.log({
                source: 'tool',
                action: 'execute',
                tool: toolName,
            });
            const emitProgress = (text) => {
                onUpdate?.({
                    content: [{ type: 'text', text }],
                    details: {},
                });
            };
            emitProgress('Regenerating SCIP index...');
            try {
                await runIndexGeneration(toolName, signal, emitProgress);
                return {
                    content: [{ type: 'text', text: 'SCIP index regenerated successfully.' }],
                    details: { success: true },
                };
            }
            catch (error) {
                const message = error instanceof Error ? error.message : String(error);
                return {
                    content: [{ type: 'text', text: `Failed to regenerate index: ${message}` }],
                    details: { success: false, error: message },
                };
            }
        },
    });
}
//# sourceMappingURL=extension.js.map