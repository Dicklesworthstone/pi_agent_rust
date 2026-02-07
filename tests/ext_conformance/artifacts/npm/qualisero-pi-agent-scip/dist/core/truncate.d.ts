/**
 * Truncation utilities for tool outputs.
 * Based on pi's built-in truncation patterns.
 *
 * Truncation uses two independent limits - whichever is hit first wins:
 * - Line limit (default: 2000 lines)
 * - Byte limit (default: 50KB)
 */
export declare const DEFAULT_MAX_LINES = 2000;
export declare const DEFAULT_MAX_BYTES: number;
export interface TruncationResult {
    content: string;
    truncated: boolean;
    truncatedBy: 'lines' | 'bytes' | null;
    totalLines: number;
    totalBytes: number;
    outputLines: number;
    outputBytes: number;
    maxLines: number;
    maxBytes: number;
}
export interface TruncationOptions {
    maxLines?: number;
    maxBytes?: number;
}
/**
 * Format bytes as human-readable size.
 */
export declare function formatSize(bytes: number): string;
/**
 * Truncate content from the head (keep first N lines/bytes).
 * Suitable for search results where you want to see the first matches.
 */
export declare function truncateHead(content: string, options?: TruncationOptions): TruncationResult;
/**
 * Build a truncation notice message for the LLM.
 */
export declare function buildTruncationNotice(result: TruncationResult, toolName: string): string;
//# sourceMappingURL=truncate.d.ts.map