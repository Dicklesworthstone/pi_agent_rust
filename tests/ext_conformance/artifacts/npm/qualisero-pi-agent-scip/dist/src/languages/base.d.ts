export interface InstallOptions {
    confirm?: (message: string) => Promise<boolean>;
}
export interface IndexOptions {
    projectRoot: string;
    outputPath: string;
    incremental?: boolean;
    signal?: AbortSignal;
    onProgress?: (message: string) => void;
}
export interface LanguageAdapter {
    name: string;
    extensions: string[];
    isIndexerAvailable(projectRoot: string): Promise<boolean>;
    installIndexer(projectRoot: string, options?: InstallOptions): Promise<void>;
    generateIndex(options: IndexOptions): Promise<void>;
    getIndexerVersion(projectRoot: string): Promise<string>;
}
//# sourceMappingURL=base.d.ts.map