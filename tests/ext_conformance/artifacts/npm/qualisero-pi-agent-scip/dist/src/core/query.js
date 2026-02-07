import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { scip } from '@sourcegraph/scip-typescript/dist/src/scip.js';
import { parseScipSymbol, roleDescription, roleIsDefinition } from './symbols.js';
export class NeedsReindexError extends Error {
    constructor(message) {
        super(`SCIP index appears corrupted or outdated: ${message}`);
        this.name = 'NeedsReindexError';
    }
}
export class ScipQuery {
    constructor(projectRoot) {
        this.projectRoot = projectRoot;
        this.index = null;
    }
    get indexPath() {
        return join(this.projectRoot, '.scip', 'index.scip');
    }
    async indexExists() {
        try {
            await readFile(this.indexPath);
            return true;
        }
        catch {
            return false;
        }
    }
    async loadIndex() {
        if (this.index)
            return;
        try {
            const data = await readFile(this.indexPath);
            this.index = scip.Index.deserializeBinary(data);
        }
        catch (error) {
            this.index = null;
            throw new NeedsReindexError(error instanceof Error ? error.message : 'Unknown error');
        }
    }
    clearCache() {
        this.index = null;
    }
    async findDefinition(symbol, contextFile) {
        await this.loadIndex();
        if (!this.index)
            return [];
        const normalized = this.normalizeSymbol(symbol);
        const documents = this.index.documents ?? [];
        const definitions = [];
        for (const document of documents) {
            const relativePath = document?.relative_path ?? '';
            const occurrences = document?.occurrences ?? [];
            for (const occurrence of occurrences) {
                const scipSymbol = occurrence?.symbol ?? '';
                if (!this.matchesSymbol(scipSymbol, normalized, contextFile, relativePath))
                    continue;
                const roles = occurrence?.symbol_roles ?? 0;
                if (!roleIsDefinition(roles))
                    continue;
                const [line = 0, character = 0] = occurrence?.range ?? [];
                definitions.push({
                    symbol: scipSymbol,
                    file: relativePath,
                    line,
                    character,
                    snippet: await this.getCodeSnippet(relativePath, line),
                });
            }
        }
        return definitions;
    }
    async findReferences(symbol) {
        await this.loadIndex();
        if (!this.index)
            return [];
        const normalized = this.normalizeSymbol(symbol);
        const documents = this.index.documents ?? [];
        const references = [];
        for (const document of documents) {
            const relativePath = document?.relative_path ?? '';
            const occurrences = document?.occurrences ?? [];
            for (const occurrence of occurrences) {
                const scipSymbol = occurrence?.symbol ?? '';
                if (!this.matchesSymbol(scipSymbol, normalized, undefined, relativePath))
                    continue;
                const [line = 0, character = 0] = occurrence?.range ?? [];
                const roles = occurrence?.symbol_roles ?? 0;
                references.push({
                    symbol: scipSymbol,
                    file: relativePath,
                    line,
                    character,
                    role: roleDescription(roles),
                });
            }
        }
        return references;
    }
    async listSymbols(file) {
        await this.loadIndex();
        if (!this.index)
            return [];
        const documents = this.index.documents ?? [];
        const target = documents.find((doc) => doc?.relative_path === file);
        if (!target)
            return [];
        const occurrences = target.occurrences ?? [];
        const seen = new Map();
        for (const occurrence of occurrences) {
            const scipSymbol = occurrence?.symbol ?? '';
            const roles = occurrence?.symbol_roles ?? 0;
            if (!roleIsDefinition(roles))
                continue;
            if (seen.has(scipSymbol))
                continue;
            const [line = 0, character = 0] = occurrence?.range ?? [];
            const parsed = parseScipSymbol(scipSymbol);
            seen.set(scipSymbol, {
                symbol: scipSymbol,
                name: parsed.name,
                kind: parsed.kind,
                file,
                line,
                character,
            });
        }
        return Array.from(seen.values());
    }
    async searchSymbols(query) {
        await this.loadIndex();
        if (!this.index)
            return [];
        const documents = this.index.documents ?? [];
        const needle = this.normalizeSymbol(query);
        const results = [];
        for (const document of documents) {
            const relativePath = document?.relative_path ?? '';
            const occurrences = document?.occurrences ?? [];
            for (const occurrence of occurrences) {
                const scipSymbol = occurrence?.symbol ?? '';
                if (!scipSymbol)
                    continue;
                const parsed = parseScipSymbol(scipSymbol);
                if (!this.normalizeSymbol(parsed.name).includes(needle))
                    continue;
                const [line = 0, character = 0] = occurrence?.range ?? [];
                results.push({
                    symbol: scipSymbol,
                    name: parsed.name,
                    kind: parsed.kind,
                    file: relativePath,
                    line,
                    character,
                });
            }
        }
        return results;
    }
    async buildProjectTree() {
        await this.loadIndex();
        if (!this.index)
            return [];
        const documents = this.index.documents ?? [];
        const modules = new Map();
        for (const document of documents) {
            const relativePath = document?.relative_path ?? '';
            if (!relativePath.endsWith('.py'))
                continue;
            const moduleName = this.pathToModuleName(relativePath);
            let moduleNode = modules.get(moduleName);
            if (!moduleNode) {
                moduleNode = {
                    kind: 'Module',
                    name: moduleName,
                    file: relativePath,
                    children: [],
                };
                modules.set(moduleName, moduleNode);
            }
            const occurrences = document?.occurrences ?? [];
            const classNodes = new Map();
            const topLevelChildren = [];
            for (const occurrence of occurrences) {
                const scipSymbol = occurrence?.symbol ?? '';
                if (!scipSymbol)
                    continue;
                const roles = occurrence?.symbol_roles ?? 0;
                if (!roleIsDefinition(roles))
                    continue;
                const [line = 0, character = 0] = occurrence?.range ?? [];
                const parsed = parseScipSymbol(scipSymbol);
                if (parsed.kind === 'Class') {
                    const classNode = {
                        kind: 'Class',
                        name: parsed.name,
                        file: relativePath,
                        line,
                        character,
                        children: [],
                    };
                    classNodes.set(parsed.name, classNode);
                    topLevelChildren.push(classNode);
                }
                else if (parsed.kind === 'Method') {
                    const className = this.extractClassFromSymbol(scipSymbol);
                    let parent;
                    if (className) {
                        parent = classNodes.get(className);
                        if (!parent) {
                            parent = {
                                kind: 'Class',
                                name: className,
                                file: relativePath,
                                children: [],
                            };
                            classNodes.set(className, parent);
                            moduleNode.children.push(parent);
                        }
                    }
                    else {
                        parent = moduleNode;
                    }
                    parent.children.push({
                        kind: 'Method',
                        name: parsed.name,
                        file: relativePath,
                        line,
                        character,
                        children: [],
                    });
                }
                else if (parsed.kind === 'Function') {
                    topLevelChildren.push({
                        kind: 'Function',
                        name: parsed.name,
                        file: relativePath,
                        line,
                        character,
                        children: [],
                    });
                }
            }
            // Ensure deterministic ordering: classes and functions sorted by name.
            topLevelChildren.sort((a, b) => a.name.localeCompare(b.name));
            moduleNode.children.push(...topLevelChildren);
        }
        return Array.from(modules.values());
    }
    pathToModuleName(path) {
        const withoutExt = path.replace(/\.py$/, '');
        const parts = withoutExt.split(/[\\/]+/);
        if (parts[0] === 'src') {
            parts.shift();
        }
        return parts.join('.');
    }
    extractClassFromSymbol(symbol) {
        // scip-python encodes methods as ...`pkg.mod`/Class#method().
        const backtickIdx = symbol.lastIndexOf('`');
        if (backtickIdx === -1)
            return null;
        const afterBacktick = symbol.slice(backtickIdx + 1);
        const slashIdx = afterBacktick.indexOf('/');
        if (slashIdx === -1)
            return null;
        const descriptor = afterBacktick.slice(slashIdx + 1);
        const hashIdx = descriptor.indexOf('#');
        if (hashIdx === -1)
            return null;
        return descriptor.slice(0, hashIdx) || null;
    }
    normalizeSymbol(raw) {
        return raw.trim().toLowerCase();
    }
    matchesSymbol(scipSymbol, normalizedQuery, contextFile, relativePath) {
        if (!scipSymbol)
            return false;
        const normalizedSymbol = this.normalizeSymbol(scipSymbol);
        if (contextFile && relativePath && contextFile === relativePath) {
            return normalizedSymbol.includes(normalizedQuery);
        }
        return normalizedSymbol.includes(normalizedQuery);
    }
    async getCodeSnippet(relativePath, line) {
        if (!relativePath)
            return '';
        try {
            const fullPath = join(this.projectRoot, relativePath);
            const content = await readFile(fullPath, 'utf8');
            const lines = content.split(/\r?\n/);
            return lines[line] ?? '';
        }
        catch {
            return '';
        }
    }
}
//# sourceMappingURL=query.js.map