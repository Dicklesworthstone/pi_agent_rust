import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createRequire } from 'node:module';
import { promises as fs } from 'node:fs';
import { join } from 'node:path';
const execFileAsync = promisify(execFile);
const require = createRequire(import.meta.url);
const SCIP_TS_ENTRY = require.resolve('@sourcegraph/scip-typescript/dist/src/main.js');
export class TypeScriptAdapter {
    constructor() {
        this.name = 'typescript';
        this.extensions = ['.ts', '.tsx', '.js', '.jsx'];
    }
    async isIndexerAvailable(projectRoot) {
        try {
            await execFileAsync(process.execPath, [SCIP_TS_ENTRY, '--version'], { cwd: projectRoot });
            return true;
        }
        catch {
            return false;
        }
    }
    async installIndexer(projectRoot, options) {
        if (options?.confirm) {
            const acknowledged = await options.confirm('scip-typescript ships with pi-agent-scip via npm and is invoked automatically. No additional installation is required.');
            if (!acknowledged) {
                throw new Error('scip-typescript execution cancelled by user');
            }
        }
    }
    async generateIndex(options) {
        const { projectRoot, outputPath, signal, onProgress } = options;
        // Detect workspace type and tsconfig presence
        const workspaceFlags = await this.detectWorkspaceFlags(projectRoot);
        const hasTsconfig = await this.hasTsconfig(projectRoot);
        const args = ['index', '--output', outputPath, ...workspaceFlags];
        // If no tsconfig.json exists, let scip-typescript infer one
        if (!hasTsconfig) {
            args.push('--infer-tsconfig');
        }
        onProgress?.('Analyzing TypeScript/JavaScript project via scip-typescript...');
        const child = spawn(process.execPath, [SCIP_TS_ENTRY, ...args], { cwd: projectRoot });
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
                    onProgress?.('TypeScript/JavaScript indexing complete');
                    resolve();
                }
                else {
                    reject(new Error(`Indexing failed with code ${code}. ` +
                        'Ensure your project has a tsconfig.json or valid JavaScript/TypeScript files.'));
                }
            });
            child.on('error', (err) => {
                reject(new Error(`Failed to run scip-typescript: ${err.message}. ` +
                    'Ensure Node.js 18+ is installed and @sourcegraph/scip-typescript is available.'));
            });
        });
    }
    async getIndexerVersion(projectRoot) {
        const { stdout } = await execFileAsync(process.execPath, [SCIP_TS_ENTRY, '--version'], {
            cwd: projectRoot,
        });
        return stdout.toString().trim();
    }
    async hasTsconfig(projectRoot) {
        try {
            await fs.access(join(projectRoot, 'tsconfig.json'));
            return true;
        }
        catch {
            return false;
        }
    }
    /** @internal Exposed for testing */
    async detectWorkspaceFlags(projectRoot) {
        // Check for pnpm workspaces
        try {
            await fs.access(join(projectRoot, 'pnpm-workspace.yaml'));
            return ['--pnpm-workspaces'];
        }
        catch {
            // not pnpm
        }
        // Check for workspaces in package.json
        try {
            const pkgPath = join(projectRoot, 'package.json');
            const content = await fs.readFile(pkgPath, 'utf-8');
            const pkg = JSON.parse(content);
            if (pkg.workspaces) {
                // Check packageManager to determine how to handle workspaces
                const pm = pkg.packageManager;
                if (pm?.startsWith('bun')) {
                    // Bun workspaces: scip-typescript doesn't have native support,
                    // but works fine without flags (uses root tsconfig with includes)
                    return [];
                }
                if (pm?.startsWith('npm')) {
                    // npm workspaces: scip-typescript doesn't have --npm-workspaces,
                    // but works fine without flags (uses root tsconfig with includes)
                    return [];
                }
                // Yarn workspaces (explicit or assumed when packageManager is not set)
                return ['--yarn-workspaces'];
            }
        }
        catch {
            // no package.json or no workspaces
        }
        return [];
    }
}
//# sourceMappingURL=typescript.js.map