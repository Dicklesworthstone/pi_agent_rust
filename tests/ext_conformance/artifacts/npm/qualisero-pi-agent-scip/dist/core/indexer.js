import { promises as fs } from 'node:fs';
import { join } from 'node:path';
import { scip } from '@sourcegraph/scip-typescript/dist/src/scip.js';
import { LanguageRegistry } from '../languages/registry.js';
import { StructuredLogger } from './logger.js';
// Supported source file extensions for reindex detection
const SOURCE_EXTENSIONS = new Set(['.py', '.ts', '.tsx', '.js', '.jsx', '.mjs', '.cjs']);
const IGNORE_DIRS = new Set(['.git', 'node_modules', '.scip', '.venv', '.poetry', 'dist', 'build', 'out']);
export class ScipIndexer {
    constructor(projectRoot, logger = new StructuredLogger(projectRoot)) {
        this.projectRoot = projectRoot;
        this.indexPath = join(projectRoot, '.scip', 'index.scip');
        this.registry = new LanguageRegistry();
        this.logger = logger;
    }
    async indexExists() {
        try {
            await fs.access(this.indexPath);
            return true;
        }
        catch {
            return false;
        }
    }
    getIndexPath() {
        return this.indexPath;
    }
    async generateIndex(options = {}) {
        const incremental = options.incremental ?? false;
        if (incremental) {
            const needs = await this.needsReindex();
            if (!needs) {
                this.logger.log({
                    source: 'indexer',
                    action: 'generate_index_skipped',
                    incremental: true,
                });
                return;
            }
        }
        this.logger.log({
            source: 'indexer',
            action: 'generate_index_start',
            incremental,
        });
        try {
            const adapters = await this.registry.detectLanguages(this.projectRoot);
            if (adapters.length === 0) {
                throw new Error('No supported language detected in project');
            }
            await this.ensureIndexDir();
            await this.backupExistingIndex();
            // If only one adapter, write directly to the output path
            if (adapters.length === 1) {
                await this.runAdapter(adapters[0], options, this.indexPath);
            }
            else {
                // Multiple adapters: write to temp files, then merge
                const tempPaths = [];
                try {
                    for (let i = 0; i < adapters.length; i++) {
                        const tempPath = `${this.indexPath}.${adapters[i].name}.tmp`;
                        tempPaths.push(tempPath);
                        await this.runAdapter(adapters[i], options, tempPath);
                    }
                    // Merge all indexes
                    await this.mergeIndexes(tempPaths, this.indexPath);
                }
                finally {
                    // Clean up temp files
                    for (const tempPath of tempPaths) {
                        try {
                            await fs.unlink(tempPath);
                        }
                        catch {
                            // Ignore cleanup errors
                        }
                    }
                }
            }
            this.logger.log({ source: 'indexer', action: 'generate_index_complete' });
        }
        catch (error) {
            this.logger.log({
                source: 'indexer',
                action: 'generate_index_failed',
                level: 'error',
                message: error instanceof Error ? error.message : String(error),
            });
            throw error;
        }
    }
    async mergeIndexes(inputPaths, outputPath) {
        const allDocuments = [];
        const seenPaths = new Set();
        let metadata = null;
        const externalSymbols = [];
        for (const inputPath of inputPaths) {
            try {
                const data = await fs.readFile(inputPath);
                const index = scip.Index.deserializeBinary(data);
                // Keep first metadata
                if (!metadata && index.metadata) {
                    metadata = index.metadata;
                }
                // Merge documents (avoid duplicates by path)
                for (const doc of index.documents || []) {
                    const path = doc.relative_path || '';
                    if (!seenPaths.has(path)) {
                        seenPaths.add(path);
                        allDocuments.push(doc);
                    }
                }
                // Merge external symbols
                for (const sym of index.external_symbols || []) {
                    externalSymbols.push(sym);
                }
            }
            catch (error) {
                // Skip indexes that can't be read
                this.logger.log({
                    source: 'indexer',
                    action: 'merge_skip',
                    path: inputPath,
                    level: 'warning',
                    message: error instanceof Error ? error.message : String(error),
                });
            }
        }
        const mergedIndex = new scip.Index({
            metadata,
            documents: allDocuments,
            external_symbols: externalSymbols,
        });
        await fs.writeFile(outputPath, Buffer.from(mergedIndex.serializeBinary()));
        this.logger.log({
            source: 'indexer',
            action: 'merge_complete',
            documents: allDocuments.length,
        });
    }
    async needsReindex() {
        // If there is no index at all, we clearly need one.
        const exists = await this.indexExists();
        if (!exists)
            return true;
        try {
            const indexStat = await fs.stat(this.indexPath);
            const indexMtime = indexStat.mtimeMs;
            const newestSourceMtime = await this.findNewestSourceMtime(this.projectRoot);
            if (newestSourceMtime === null) {
                // No source files found; keep existing index.
                return false;
            }
            return newestSourceMtime > indexMtime;
        }
        catch {
            // On any error while checking mtimes, err on the side of reindexing.
            return true;
        }
    }
    async findNewestSourceMtime(root) {
        let newest = null;
        let entries;
        try {
            entries = await fs.readdir(root, { withFileTypes: true });
        }
        catch {
            return null;
        }
        for (const entry of entries) {
            if (IGNORE_DIRS.has(entry.name) || entry.name.startsWith('.')) {
                continue;
            }
            const fullPath = join(root, entry.name);
            if (entry.isDirectory()) {
                const childNewest = await this.findNewestSourceMtime(fullPath);
                if (childNewest !== null && (newest === null || childNewest > newest)) {
                    newest = childNewest;
                }
            }
            else if (entry.isFile()) {
                const ext = this.getExtension(entry.name);
                if (SOURCE_EXTENSIONS.has(ext)) {
                    const stat = await fs.stat(fullPath);
                    const mtime = stat.mtimeMs;
                    if (newest === null || mtime > newest) {
                        newest = mtime;
                    }
                }
            }
        }
        return newest;
    }
    getExtension(filename) {
        const lastDot = filename.lastIndexOf('.');
        if (lastDot === -1)
            return '';
        return filename.slice(lastDot);
    }
    async runAdapter(adapter, options, outputPath) {
        options.onProgress?.(`Detected ${adapter.name} project`);
        this.logger.log({ source: 'indexer', action: 'adapter_start', adapter: adapter.name });
        if (!(await adapter.isIndexerAvailable(this.projectRoot))) {
            options.onProgress?.(`Preparing ${adapter.name} indexer via npm...`);
            this.logger.log({ source: 'indexer', action: 'adapter_install', adapter: adapter.name });
            await adapter.installIndexer(this.projectRoot, {
                confirm: options.confirmInstall,
            });
        }
        try {
            await adapter.generateIndex({
                projectRoot: this.projectRoot,
                outputPath,
                incremental: options.incremental,
                signal: options.signal,
                onProgress: options.onProgress,
            });
            this.logger.log({ source: 'indexer', action: 'adapter_complete', adapter: adapter.name });
        }
        catch (error) {
            this.logger.log({
                source: 'indexer',
                action: 'adapter_failed',
                adapter: adapter.name,
                level: 'error',
                message: error instanceof Error ? error.message : String(error),
            });
            throw error;
        }
    }
    async ensureIndexDir() {
        const scipDir = join(this.projectRoot, '.scip');
        const isFirstRun = !(await fs.access(scipDir).then(() => true).catch(() => false));
        await fs.mkdir(scipDir, { recursive: true });
        // On first run, ensure .scip is in .gitignore
        if (isFirstRun) {
            await this.ensureGitignore();
        }
    }
    async ensureGitignore() {
        const gitignorePath = join(this.projectRoot, '.gitignore');
        try {
            // Check if .git directory exists (i.e., this is a git repo)
            const isGitRepo = await fs.access(join(this.projectRoot, '.git')).then(() => true).catch(() => false);
            if (!isGitRepo)
                return;
            let content = '';
            let exists = false;
            try {
                content = await fs.readFile(gitignorePath, 'utf-8');
                exists = true;
            }
            catch {
                // .gitignore doesn't exist
            }
            // Check if .scip is already ignored (handles .scip, .scip/, /.scip, etc.)
            const lines = content.split('\n');
            const alreadyIgnored = lines.some(line => {
                const trimmed = line.trim();
                return trimmed === '.scip' || trimmed === '.scip/' || trimmed === '/.scip' || trimmed === '/.scip/';
            });
            if (!alreadyIgnored) {
                const newEntry = '.scip/';
                const newContent = exists
                    ? (content.endsWith('\n') ? content + newEntry + '\n' : content + '\n' + newEntry + '\n')
                    : newEntry + '\n';
                await fs.writeFile(gitignorePath, newContent);
                this.logger.log({ source: 'indexer', action: 'gitignore_updated', path: gitignorePath });
            }
        }
        catch (error) {
            // Don't fail indexing if gitignore update fails
            this.logger.log({
                source: 'indexer',
                action: 'gitignore_update_failed',
                level: 'warning',
                message: error instanceof Error ? error.message : String(error),
            });
        }
    }
    async backupExistingIndex() {
        try {
            await fs.copyFile(this.indexPath, `${this.indexPath}.bak`);
        }
        catch {
            // ignore when source does not exist
        }
    }
}
//# sourceMappingURL=indexer.js.map