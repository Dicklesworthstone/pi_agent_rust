import type { InstallOptions, IndexOptions, LanguageAdapter } from './base.js';
export declare class PythonAdapter implements LanguageAdapter {
    readonly name = "python";
    readonly extensions: string[];
    isIndexerAvailable(projectRoot: string): Promise<boolean>;
    installIndexer(projectRoot: string, options?: InstallOptions): Promise<void>;
    generateIndex(options: IndexOptions): Promise<void>;
    getIndexerVersion(projectRoot: string): Promise<string>;
}
//# sourceMappingURL=python.d.ts.map