import { promises as fs } from 'node:fs';
import { join } from 'node:path';
// Cache detected languages per cwd to avoid re-scanning on every agent start
const languageCache = new Map();
async function detectLanguages(cwd) {
    const cached = languageCache.get(cwd);
    if (cached)
        return cached;
    const result = { python: false, typescript: false };
    try {
        // Check for Python
        const pyproject = join(cwd, 'pyproject.toml');
        const setup = join(cwd, 'setup.py');
        const hasPyproject = await fs.access(pyproject).then(() => true).catch(() => false);
        const hasSetup = await fs.access(setup).then(() => true).catch(() => false);
        if (hasPyproject || hasSetup) {
            result.python = true;
        }
        else {
            // Fallback: shallow scan for .py files in src/ and root
            const srcDir = join(cwd, 'src');
            const srcEntries = await fs.readdir(srcDir).catch(() => []);
            if (srcEntries.some((e) => e.endsWith('.py'))) {
                result.python = true;
            }
            else {
                const rootEntries = await fs.readdir(cwd);
                if (rootEntries.some((e) => e.endsWith('.py'))) {
                    result.python = true;
                }
            }
        }
        // Check for TypeScript/JavaScript
        const tsconfig = join(cwd, 'tsconfig.json');
        const jsconfig = join(cwd, 'jsconfig.json');
        const packageJson = join(cwd, 'package.json');
        const hasTsconfig = await fs.access(tsconfig).then(() => true).catch(() => false);
        const hasJsconfig = await fs.access(jsconfig).then(() => true).catch(() => false);
        if (hasTsconfig || hasJsconfig) {
            result.typescript = true;
        }
        else {
            // Check package.json for TypeScript dependency
            try {
                const content = await fs.readFile(packageJson, 'utf-8');
                const pkg = JSON.parse(content);
                const deps = { ...pkg.dependencies, ...pkg.devDependencies };
                if (deps['typescript']) {
                    result.typescript = true;
                }
            }
            catch {
                // No package.json or invalid JSON
            }
            // Fallback: shallow scan for .ts/.tsx files
            if (!result.typescript) {
                const srcDir = join(cwd, 'src');
                const srcEntries = await fs.readdir(srcDir).catch(() => []);
                if (srcEntries.some((e) => e.endsWith('.ts') || e.endsWith('.tsx'))) {
                    result.typescript = true;
                }
                else {
                    const rootEntries = await fs.readdir(cwd);
                    if (rootEntries.some((e) => e.endsWith('.ts') || e.endsWith('.tsx'))) {
                        result.typescript = true;
                    }
                }
            }
        }
    }
    catch {
        // Ignore errors, return defaults
    }
    languageCache.set(cwd, result);
    return result;
}
// Track whether we've already injected a message for this session
let messageInjected = false;
export default function (pi) {
    // Reset flag on session start
    pi.on('session_start', async () => {
        messageInjected = false;
    });
    // Inject guidance message before the first agent turn
    pi.on('before_agent_start', async (_event, ctx) => {
        if (messageInjected)
            return;
        const languages = await detectLanguages(ctx.cwd);
        const hasAnyLanguage = languages.python || languages.typescript;
        if (!hasAnyLanguage)
            return;
        // Check if scip tools are available by looking at session tools
        // Note: We can't easily check tools here, so we inject the message
        // and let the agent figure out if the tools are available
        const languageNames = [];
        if (languages.python)
            languageNames.push('Python');
        if (languages.typescript)
            languageNames.push('TypeScript/JavaScript');
        const languageList = languageNames.join(' and ');
        messageInjected = true;
        return {
            message: {
                customType: 'pi-agent-scip-hint',
                content: `For this ${languageList} project, prefer the scip_* tools from @qualisero/pi-agent-scip for code navigation and structure: ` +
                    'use scip_find_definition, scip_find_references, scip_list_symbols, scip_search_symbols, and scip_project_tree ' +
                    'instead of ad-hoc text search or manual file scanning.',
                display: false, // Don't clutter the UI, just send to LLM
            },
        };
    });
}
//# sourceMappingURL=hook.js.map