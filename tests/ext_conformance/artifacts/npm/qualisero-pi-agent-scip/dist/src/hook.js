import { promises as fs } from 'node:fs';
import { join } from 'node:path';
async function isPythonRepo(cwd) {
    try {
        const pyproject = join(cwd, 'pyproject.toml');
        const setup = join(cwd, 'setup.py');
        const hasPyproject = await fs
            .access(pyproject)
            .then(() => true)
            .catch(() => false);
        if (hasPyproject)
            return true;
        const hasSetup = await fs
            .access(setup)
            .then(() => true)
            .catch(() => false);
        if (hasSetup)
            return true;
        // Fallback: shallow scan for .py files in src/ and root
        const srcDir = join(cwd, 'src');
        const srcEntries = await fs.readdir(srcDir).catch(() => []);
        if (srcEntries.some((e) => e.endsWith('.py')))
            return true;
        const rootEntries = await fs.readdir(cwd);
        if (rootEntries.some((e) => e.endsWith('.py')))
            return true;
        return false;
    }
    catch {
        return false;
    }
}
const factory = (pi) => {
    pi.on('session_start', async (_event, ctx) => {
        const python = await isPythonRepo(ctx.cwd);
        if (!python)
            return;
        const tools = ctx.tools ?? [];
        const hasScip = tools.some((t) => t.name && t.name.startsWith('scip_'));
        if (!hasScip)
            return;
        ctx.addSystemMessage('For this Python project, prefer the scip_* tools from @qualisero/pi-scip for code navigation and structure: ' +
            'use scip_find_definition, scip_find_references, scip_list_symbols, scip_search_symbols, and scip_project_tree ' +
            'instead of ad-hoc text search or manual file scanning.');
    });
};
export default factory;
//# sourceMappingURL=hook.js.map