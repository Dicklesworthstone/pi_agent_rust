import type { InstallOptions, IndexOptions, LanguageAdapter } from './base.js';
export declare class TypeScriptAdapter implements LanguageAdapter {
    readonly name = "typescript";
    readonly extensions: string[];
    isIndexerAvailable(projectRoot: string): Promise<boolean>;
    installIndexer(projectRoot: string, options?: InstallOptions): Promise<void>;
    generateIndex(options: IndexOptions): Promise<void>;
    getIndexerVersion(projectRoot: string): Promise<string>;
    private hasTsconfig;
    /** @internal Exposed for testing */
    detectWorkspaceFlags(projectRoot: string): Promise<string[]>;
}
//# sourceMappingURL=typescript.d.ts.map