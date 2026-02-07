export type LogLevel = 'info' | 'error';
export interface StructuredLogEvent {
    source: 'indexer' | 'tool';
    action: string;
    level?: LogLevel;
    message?: string;
    [key: string]: unknown;
}
export interface StructuredLoggerOptions {
    enableConsole?: boolean;
    enableFile?: boolean;
}
export declare class StructuredLogger {
    private readonly projectRoot;
    private readonly logPath;
    private readonly enableConsole;
    private readonly enableFile;
    private writeChain;
    constructor(projectRoot: string, options?: StructuredLoggerOptions);
    log(event: StructuredLogEvent): void;
}
//# sourceMappingURL=logger.d.ts.map