#!/usr/bin/env node
import { existsSync, statSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { ScipIndexer } from '../core/indexer.js';
import { ScipQuery } from '../core/query.js';
import { PythonAdapter } from '../languages/python.js';
export async function runStatus(cwd = process.cwd()) {
    const indexer = new ScipIndexer(cwd);
    const query = new ScipQuery(cwd);
    const pythonAdapter = new PythonAdapter();
    const indexPath = join(cwd, '.scip', 'index.scip');
    const logPath = join(cwd, '.scip', 'index.log');
    console.log('SCIP status for project:', cwd);
    const hasIndex = existsSync(indexPath);
    console.log('- Index file:', hasIndex ? indexPath : 'missing');
    if (hasIndex) {
        const stats = statSync(indexPath);
        console.log(`  size: ${stats.size} bytes`);
    }
    console.log('- Indexer available (python):', await pythonAdapter.isIndexerAvailable(cwd));
    console.log('- Queryable index:', await query.indexExists());
    if (existsSync(logPath)) {
        const content = readFileSync(logPath, 'utf8').trim().split(/\r?\n/);
        const last = content[content.length - 1];
        console.log('- Last log entry:', last);
    }
    else {
        console.log('- Last log entry: none (no log file)');
    }
}
if (import.meta.url === `file://${process.argv[1]}`) {
    runStatus().catch((err) => {
        console.error('Status command failed:', err.message ?? err);
        process.exit(1);
    });
}
//# sourceMappingURL=status.js.map