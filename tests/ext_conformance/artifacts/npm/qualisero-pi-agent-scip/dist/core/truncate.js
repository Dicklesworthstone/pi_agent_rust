/**
 * Truncation utilities for tool outputs.
 * Based on pi's built-in truncation patterns.
 *
 * Truncation uses two independent limits - whichever is hit first wins:
 * - Line limit (default: 2000 lines)
 * - Byte limit (default: 50KB)
 */
export const DEFAULT_MAX_LINES = 2000;
export const DEFAULT_MAX_BYTES = 50 * 1024; // 50KB
/**
 * Format bytes as human-readable size.
 */
export function formatSize(bytes) {
    if (bytes < 1024) {
        return `${bytes}B`;
    }
    else if (bytes < 1024 * 1024) {
        return `${(bytes / 1024).toFixed(1)}KB`;
    }
    else {
        return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
    }
}
/**
 * Truncate content from the head (keep first N lines/bytes).
 * Suitable for search results where you want to see the first matches.
 */
export function truncateHead(content, options = {}) {
    const maxLines = options.maxLines ?? DEFAULT_MAX_LINES;
    const maxBytes = options.maxBytes ?? DEFAULT_MAX_BYTES;
    const totalBytes = Buffer.byteLength(content, 'utf-8');
    const lines = content.split('\n');
    const totalLines = lines.length;
    // Check if no truncation needed
    if (totalLines <= maxLines && totalBytes <= maxBytes) {
        return {
            content,
            truncated: false,
            truncatedBy: null,
            totalLines,
            totalBytes,
            outputLines: totalLines,
            outputBytes: totalBytes,
            maxLines,
            maxBytes,
        };
    }
    // Collect complete lines that fit
    const outputLinesArr = [];
    let outputBytesCount = 0;
    let truncatedBy = 'lines';
    for (let i = 0; i < lines.length && i < maxLines; i++) {
        const line = lines[i];
        const lineBytes = Buffer.byteLength(line, 'utf-8') + (i > 0 ? 1 : 0); // +1 for newline
        if (outputBytesCount + lineBytes > maxBytes) {
            truncatedBy = 'bytes';
            break;
        }
        outputLinesArr.push(line);
        outputBytesCount += lineBytes;
    }
    // If we exited due to line limit
    if (outputLinesArr.length >= maxLines && outputBytesCount <= maxBytes) {
        truncatedBy = 'lines';
    }
    const outputContent = outputLinesArr.join('\n');
    const finalOutputBytes = Buffer.byteLength(outputContent, 'utf-8');
    return {
        content: outputContent,
        truncated: true,
        truncatedBy,
        totalLines,
        totalBytes,
        outputLines: outputLinesArr.length,
        outputBytes: finalOutputBytes,
        maxLines,
        maxBytes,
    };
}
/**
 * Build a truncation notice message for the LLM.
 */
export function buildTruncationNotice(result, toolName) {
    if (!result.truncated)
        return '';
    const limitHit = result.truncatedBy === 'lines'
        ? `${result.maxLines} lines`
        : formatSize(result.maxBytes);
    return `\n\n[Output truncated: showing ${result.outputLines} of ${result.totalLines} lines (${formatSize(result.outputBytes)} of ${formatSize(result.totalBytes)}). ${limitHit} limit reached. Refine your ${toolName} query for more targeted results.]`;
}
//# sourceMappingURL=truncate.js.map