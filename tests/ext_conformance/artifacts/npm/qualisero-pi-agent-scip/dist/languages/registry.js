import { promises as fs } from 'node:fs';
import { join } from 'node:path';
import { PythonAdapter } from './python.js';
import { TypeScriptAdapter } from './typescript.js';
const DEFAULT_IGNORE = new Set(['.git', '.scip', 'node_modules', '.venv', '.poetry', 'dist', 'build', 'out']);
export class LanguageRegistry {
    async detectLanguages(projectRoot) {
        const adapters = [];
        // Check for TypeScript/JavaScript projects
        if (await this.isTypeScriptProject(projectRoot)) {
            adapters.push(new TypeScriptAdapter());
        }
        // Check for Python projects
        if (await this.isPythonProject(projectRoot)) {
            adapters.push(new PythonAdapter());
        }
        return adapters;
    }
    async isTypeScriptProject(projectRoot) {
        // Check for tsconfig.json (definitive TypeScript indicator)
        try {
            await fs.access(join(projectRoot, 'tsconfig.json'));
            return true;
        }
        catch {
            // continue checking
        }
        // Check for jsconfig.json (JavaScript project with type checking)
        try {
            await fs.access(join(projectRoot, 'jsconfig.json'));
            return true;
        }
        catch {
            // continue checking
        }
        // Check for package.json (indicates Node.js project, may have TS/JS)
        try {
            const pkgPath = join(projectRoot, 'package.json');
            const content = await fs.readFile(pkgPath, 'utf-8');
            const pkg = JSON.parse(content);
            // Has TypeScript as dependency
            const deps = { ...pkg.dependencies, ...pkg.devDependencies };
            if (deps['typescript']) {
                return true;
            }
            // Has a main entry pointing to JS/TS
            if (pkg.main && /\.(js|ts|mjs|cjs)$/.test(pkg.main)) {
                return true;
            }
        }
        catch {
            // continue checking
        }
        // Fallback: check for .ts/.tsx files in src/ or root
        const tsExtensions = ['.ts', '.tsx'];
        for (const ext of tsExtensions) {
            if (await this.hasFiles(projectRoot, ext)) {
                return true;
            }
        }
        return false;
    }
    async isPythonProject(projectRoot) {
        // Check for pyproject.toml
        try {
            await fs.access(join(projectRoot, 'pyproject.toml'));
            return true;
        }
        catch {
            // continue checking
        }
        // Check for setup.py
        try {
            await fs.access(join(projectRoot, 'setup.py'));
            return true;
        }
        catch {
            // continue checking
        }
        // Check for requirements.txt
        try {
            await fs.access(join(projectRoot, 'requirements.txt'));
            return true;
        }
        catch {
            // continue checking
        }
        // Fallback: check for .py files
        if (await this.hasFiles(projectRoot, '.py')) {
            return true;
        }
        return false;
    }
    async hasFiles(root, extension) {
        const queue = [root];
        while (queue.length) {
            const current = queue.pop();
            let entries;
            try {
                entries = await fs.readdir(current, { withFileTypes: true });
            }
            catch {
                continue;
            }
            for (const entry of entries) {
                if (entry.name.startsWith('.'))
                    continue;
                if (DEFAULT_IGNORE.has(entry.name))
                    continue;
                const fullPath = join(current, entry.name);
                if (entry.isDirectory()) {
                    queue.push(fullPath);
                }
                else if (entry.isFile() && entry.name.endsWith(extension)) {
                    return true;
                }
            }
        }
        return false;
    }
}
//# sourceMappingURL=registry.js.map