import { StructuredLogger } from './logger.js';
export interface GenerateOptions {
    incremental?: boolean;
    signal?: AbortSignal;
    onProgress?: (message: string) => void;
    confirmInstall?: (message: string) => Promise<boolean>;
}
export declare class ScipIndexer {
    private readonly projectRoot;
    private readonly indexPath;
    private readonly registry;
    private readonly logger;
    constructor(projectRoot: string, logger?: StructuredLogger);
    indexExists(): Promise<boolean>;
    getIndexPath(): string;
    generateIndex(options?: GenerateOptions): Promise<void>;
    private mergeIndexes;
    needsReindex(): Promise<boolean>;
    private findNewestSourceMtime;
    private getExtension;
    private runAdapter;
    private ensureIndexDir;
    private ensureGitignore;
    private backupExistingIndex;
}
//# sourceMappingURL=indexer.d.ts.map