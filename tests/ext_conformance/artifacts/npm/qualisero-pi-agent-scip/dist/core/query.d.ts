export interface Definition {
    symbol: string;
    file: string;
    line: number;
    character: number;
    snippet: string;
}
export interface Reference {
    symbol: string;
    file: string;
    line: number;
    character: number;
    role: string;
}
export interface SymbolInfo {
    symbol: string;
    name: string;
    kind: string;
    file: string;
    line: number;
    character: number;
}
export interface SearchResult extends SymbolInfo {
}
export interface CodeTreeNode {
    kind: 'Package' | 'Module' | 'Class' | 'Function' | 'Method' | 'Parameter' | 'Variable';
    name: string;
    file?: string;
    line?: number;
    character?: number;
    children: CodeTreeNode[];
}
export declare class NeedsReindexError extends Error {
    constructor(message: string);
}
export declare class ScipQuery {
    private readonly projectRoot;
    private index;
    constructor(projectRoot: string);
    get indexPath(): string;
    indexExists(): Promise<boolean>;
    loadIndex(): Promise<void>;
    clearCache(): void;
    findDefinition(symbol: string, contextFile?: string): Promise<Definition[]>;
    findReferences(symbol: string): Promise<Reference[]>;
    listSymbols(file: string): Promise<SymbolInfo[]>;
    searchSymbols(query: string): Promise<SearchResult[]>;
    buildProjectTree(): Promise<CodeTreeNode[]>;
    private isSupportedFile;
    private pathToModuleName;
    private extractClassFromSymbol;
    private normalizeSymbol;
    private matchesSymbol;
    private getCodeSnippet;
}
//# sourceMappingURL=query.d.ts.map