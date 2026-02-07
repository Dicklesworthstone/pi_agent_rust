import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createRequire } from 'node:module';
const execFileAsync = promisify(execFile);
const require = createRequire(import.meta.url);
const SCIP_PYTHON_ENTRY = require.resolve('@sourcegraph/scip-python/index.js');
export class PythonAdapter {
    constructor() {
        this.name = 'python';
        this.extensions = ['.py'];
    }
    async isIndexerAvailable(projectRoot) {
        try {
            await execFileAsync(process.execPath, [SCIP_PYTHON_ENTRY, '--version'], { cwd: projectRoot });
            return true;
        }
        catch {
            return false;
        }
    }
    async installIndexer(projectRoot, options) {
        if (options?.confirm) {
            const acknowledged = await options.confirm('scip-python ships with pi-scip via npm and is invoked automatically. No additional installation is required.');
            if (!acknowledged) {
                throw new Error('scip-python execution cancelled by user');
            }
        }
    }
    async generateIndex(options) {
        const { projectRoot, outputPath, incremental, signal, onProgress } = options;
        const args = ['index', projectRoot, '--output', outputPath];
        // Note: current scip-python version in use does not support a CLI --incremental flag.
        // We still respect the "incremental" option at the ScipIndexer level (to decide
        // whether to skip work via needsReindex()), but always invoke scip-python with a
        // full index command here.
        void incremental;
        onProgress?.('Analyzing Python project via scip-python...');
        const child = spawn(process.execPath, [SCIP_PYTHON_ENTRY, ...args], { cwd: projectRoot });
        signal?.addEventListener('abort', () => {
            child.kill();
        });
        child.stdout?.on('data', (chunk) => {
            onProgress?.(chunk.toString().trim());
        });
        child.stderr?.on('data', (chunk) => {
            onProgress?.(`[stderr] ${chunk.toString().trim()}`);
        });
        await new Promise((resolve, reject) => {
            child.on('close', (code) => {
                if (code === 0) {
                    onProgress?.('Python indexing complete');
                    resolve();
                }
                else {
                    reject(new Error(`Indexing failed with code ${code}. ` +
                        'If the index is empty or incomplete, ensure your project has a pyproject.toml ' +
                        'with [tool.pyright] or a valid Python package structure.'));
                }
            });
            child.on('error', (err) => {
                reject(new Error(`Failed to run scip-python: ${err.message}. ` +
                    'Ensure Node.js 18+ is installed and @sourcegraph/scip-python is available.'));
            });
        });
    }
    async getIndexerVersion(projectRoot) {
        const { stdout } = await execFileAsync(process.execPath, [SCIP_PYTHON_ENTRY, '--version'], {
            cwd: projectRoot,
        });
        return stdout.toString().trim();
    }
}
//# sourceMappingURL=python.js.map