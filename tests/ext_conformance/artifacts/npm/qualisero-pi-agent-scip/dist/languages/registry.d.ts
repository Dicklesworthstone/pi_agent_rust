import type { LanguageAdapter } from './base.js';
export declare class LanguageRegistry {
    detectLanguages(projectRoot: string): Promise<LanguageAdapter[]>;
    private isTypeScriptProject;
    private isPythonProject;
    private hasFiles;
}
//# sourceMappingURL=registry.d.ts.map