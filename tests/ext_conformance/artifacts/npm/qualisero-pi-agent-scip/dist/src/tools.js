import { Type } from '@sinclair/typebox';
import { ScipIndexer } from './core/indexer.js';
import { ScipQuery, NeedsReindexError } from './core/query.js';
import { StructuredLogger } from './core/logger.js';
export const createScipTools = (pi) => {
    const logger = new StructuredLogger(pi.cwd);
    const indexer = new ScipIndexer(pi.cwd, logger);
    const query = new ScipQuery(pi.cwd);
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
    return [
        // Definition tool (already wired in index.ts, kept here for reuse if needed)
        {
            name: 'scip_find_definition',
            label: 'SCIP: Find Definition',
            description: 'Locate the definition of a symbol using SCIP indexes',
            parameters: Type.Object({
                symbol: Type.String({ description: 'Symbol to find (class, function, variable)' }),
                file: Type.Optional(Type.String({ description: 'Current file path (optional)' })),
            }),
            async execute(toolCallId, params, onUpdate, ctx, signal) {
                const toolName = 'scip_find_definition';
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
                    const formatted = results
                        .map((result) => `${result.file}:${result.line + 1}:${result.character + 1}\n${result.snippet}`)
                        .join('\n\n');
                    return {
                        content: [{ type: 'text', text: formatted }],
                        details: results,
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
        },
        // References
        {
            name: 'scip_find_references',
            label: 'SCIP: Find References',
            description: 'Find all references to a symbol across the project',
            parameters: Type.Object({
                symbol: Type.String({ description: 'Symbol name to search for' }),
                limit: Type.Optional(Type.Number({ description: 'Maximum results', default: 50 })),
            }),
            async execute(toolCallId, params, onUpdate, ctx, signal) {
                const toolName = 'scip_find_references';
                logger.log({ source: 'tool', action: 'execute', tool: toolName, symbol: params.symbol });
                await ensureIndex(toolName, signal);
                const results = await query.findReferences(params.symbol);
                const limit = params.limit ?? 50;
                const limited = results.slice(0, limit);
                logger.log({ source: 'tool', action: 'query_complete', tool: toolName, hits: results.length });
                const header = `Found ${results.length} reference(s) to '${params.symbol}'` +
                    (results.length > limited.length ? ` (showing first ${limited.length})` : '');
                const body = limited
                    .map((r) => `${r.file}:${r.line + 1}:${r.character + 1} [${r.role}]`)
                    .join('\n');
                return {
                    content: [{ type: 'text', text: `${header}\n\n${body}` }],
                    details: limited,
                };
            },
        },
        // List symbols in file
        {
            name: 'scip_list_symbols',
            label: 'SCIP: List Symbols',
            description: 'List all symbols defined in a single file',
            parameters: Type.Object({
                file: Type.String({ description: 'Relative file path within the project' }),
            }),
            async execute(toolCallId, params) {
                const toolName = 'scip_list_symbols';
                logger.log({ source: 'tool', action: 'execute', tool: toolName, file: params.file });
                if (!(await query.indexExists())) {
                    return {
                        content: [{ type: 'text', text: 'SCIP index not found. Run scip_reindex first.' }],
                        details: [],
                    };
                }
                const symbols = await query.listSymbols(params.file);
                logger.log({ source: 'tool', action: 'query_complete', tool: toolName, hits: symbols.length });
                const body = symbols
                    .map((s) => `${s.kind.padEnd(10)} ${s.name} (line ${s.line + 1}, col ${s.character + 1})`)
                    .join('\n');
                return {
                    content: [{ type: 'text', text: `Symbols in ${params.file}:\n\n${body}` }],
                    details: symbols,
                };
            },
        },
        // Search symbols by name
        {
            name: 'scip_search_symbols',
            label: 'SCIP: Search Symbols',
            description: 'Search for symbols by (partial) name across the project',
            parameters: Type.Object({
                query: Type.String({ description: 'Substring to match against symbol names' }),
                limit: Type.Optional(Type.Number({ description: 'Maximum results', default: 20 })),
            }),
            async execute(toolCallId, params) {
                const toolName = 'scip_search_symbols';
                logger.log({ source: 'tool', action: 'execute', tool: toolName, query: params.query });
                if (!(await query.indexExists())) {
                    return {
                        content: [{ type: 'text', text: 'SCIP index not found. Run scip_reindex first.' }],
                        details: [],
                    };
                }
                const results = await query.searchSymbols(params.query);
                const limit = params.limit ?? 20;
                const limited = results.slice(0, limit);
                logger.log({ source: 'tool', action: 'query_complete', tool: toolName, hits: results.length });
                const body = limited
                    .map((s) => `${s.kind.padEnd(10)} ${s.name} - ${s.file}:${s.line + 1}:${s.character + 1}`)
                    .join('\n');
                return {
                    content: [{ type: 'text', text: `Found ${results.length} symbol(s) matching '${params.query}':\n\n${body}` }],
                    details: limited,
                };
            },
        },
        // Reindex tool
        {
            name: 'scip_reindex',
            label: 'SCIP: Reindex Project',
            description: 'Regenerate the SCIP index for the current project',
            parameters: Type.Object({
                incremental: Type.Optional(Type.Boolean({ description: 'Attempt incremental reindex', default: false })),
            }),
            async execute(toolCallId, params, onUpdate, ctx, signal) {
                const toolName = 'scip_reindex';
                const emitProgress = (text) => {
                    onUpdate?.({
                        content: [{ type: 'text', text }],
                        details: [],
                    });
                };
                logger.log({ source: 'tool', action: 'execute', tool: toolName, incremental: params.incremental });
                const incremental = params.incremental ?? false;
                try {
                    if (incremental) {
                        const needs = await indexer.needsReindex();
                        if (!needs) {
                            logger.log({ source: 'tool', action: 'reindex_skipped', tool: toolName, incremental: true });
                            const message = 'SCIP index is already up to date; incremental reindex skipped.';
                            emitProgress(message);
                            return {
                                content: [{ type: 'text', text: message }],
                                details: [],
                            };
                        }
                    }
                    await indexer.generateIndex({
                        incremental,
                        signal,
                        onProgress: emitProgress,
                        // Auto-accept bundled indexer usage to avoid extra prompts.
                        confirmInstall: undefined,
                    });
                    logger.log({ source: 'tool', action: 'reindex_complete', tool: toolName });
                    return {
                        content: [{ type: 'text', text: 'Reindex complete.' }],
                        details: [],
                    };
                }
                catch (error) {
                    logger.log({
                        source: 'tool',
                        action: 'reindex_failed',
                        tool: toolName,
                        level: 'error',
                        message: error instanceof Error ? error.message : String(error),
                    });
                    throw error;
                }
            },
        },
        // Project tree
        {
            name: 'scip_project_tree',
            label: 'SCIP: Project Tree',
            description: 'Summarize the code structure of the current project as a tree',
            parameters: Type.Object({
                depth: Type.Optional(Type.Number({ description: 'Maximum tree depth for text output', default: 3 })),
            }),
            async execute(toolCallId, params, onUpdate, ctx, signal) {
                const toolName = 'scip_project_tree';
                const depth = params.depth ?? 3;
                const emitProgress = (text) => {
                    onUpdate?.({
                        content: [{ type: 'text', text }],
                        details: [],
                    });
                };
                logger.log({ source: 'tool', action: 'execute', tool: toolName, depth });
                await ensureIndex(toolName, signal, emitProgress);
                const tree = await query.buildProjectTree();
                logger.log({ source: 'tool', action: 'query_complete', tool: toolName, modules: tree.length });
                const rendered = renderTree(tree, depth);
                return {
                    content: [{ type: 'text', text: rendered }],
                    details: tree,
                };
            },
        },
    ];
};
function renderTree(nodes, maxDepth) {
    const lines = [];
    const visit = (node, depth) => {
        if (depth > maxDepth)
            return;
        const prefix = '  '.repeat(depth);
        const location = node.file != null && node.line != null ? ` (${node.file}:${node.line + 1})` : '';
        lines.push(`${prefix}- [${node.kind}] ${node.name}${location}`);
        if (node.children && depth < maxDepth) {
            for (const child of node.children) {
                visit(child, depth + 1);
            }
        }
    };
    for (const node of nodes) {
        visit(node, 0);
    }
    return lines.join('\n');
}
//# sourceMappingURL=tools.js.map