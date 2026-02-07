import { promises as fs } from 'node:fs';
import { join } from 'node:path';
import { LanguageRegistry } from '../languages/registry.js';
import { StructuredLogger } from './logger.js';
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
            for (const adapter of adapters) {
                await this.runAdapter(adapter, options);
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
        const entries = await fs.readdir(root, { withFileTypes: true });
        for (const entry of entries) {
            if (entry.name === '.git' || entry.name === 'node_modules' || entry.name === '.scip' || entry.name === '.venv' || entry.name === '.poetry') {
                continue;
            }
            const fullPath = join(root, entry.name);
            if (entry.isDirectory()) {
                const childNewest = await this.findNewestSourceMtime(fullPath);
                if (childNewest !== null && (newest === null || childNewest > newest)) {
                    newest = childNewest;
                }
            }
            else if (entry.isFile() && fullPath.endsWith('.py')) {
                const stat = await fs.stat(fullPath);
                const mtime = stat.mtimeMs;
                if (newest === null || mtime > newest) {
                    newest = mtime;
                }
            }
        }
        return newest;
    }
    async runAdapter(adapter, options) {
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
                outputPath: this.indexPath,
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
        await fs.mkdir(join(this.projectRoot, '.scip'), { recursive: true });
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