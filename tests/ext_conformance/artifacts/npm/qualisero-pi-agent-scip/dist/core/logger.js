import { appendFile, mkdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
export class StructuredLogger {
    constructor(projectRoot, options = {}) {
        this.projectRoot = projectRoot;
        this.writeChain = Promise.resolve();
        this.logPath = join(projectRoot, '.scip', 'index.log');
        this.enableConsole = options.enableConsole ?? true;
        this.enableFile = options.enableFile ?? true;
    }
    log(event) {
        const payload = {
            timestamp: new Date().toISOString(),
            level: event.level ?? 'info',
            ...event,
        };
        if (this.enableConsole) {
            const prefix = `[scip][${payload.level}]`;
            const summary = payload.message ?? payload.action;
            // eslint-disable-next-line no-console
            console.log(`${prefix} ${summary}`);
        }
        if (!this.enableFile)
            return;
        const line = JSON.stringify(payload);
        this.writeChain = this.writeChain
            .then(async () => {
            await mkdir(dirname(this.logPath), { recursive: true });
            await appendFile(this.logPath, `${line}\n`);
        })
            .catch(() => {
            // Ignore write errors to avoid crashing tool execution.
        });
    }
}
//# sourceMappingURL=logger.js.map