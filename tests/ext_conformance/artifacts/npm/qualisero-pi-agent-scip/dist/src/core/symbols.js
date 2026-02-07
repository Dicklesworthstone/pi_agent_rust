import { scip } from '@sourcegraph/scip-typescript/dist/src/scip.js';
export function parseScipSymbol(symbol) {
    // scip-python examples:
    // "scip-python python myproj 0.1.0 `src.app`/Foo#bar()."
    // "scip-python python myproj 0.1.0 `src.app`/helper()."
    // "scip-python python myproj 0.1.0 `src.app`/helper().(value)"
    const trimmed = symbol.trim();
    if (!trimmed) {
        return { name: symbol, kind: 'Unknown' };
    }
    // Grab the last "descriptor" part after the final '/'
    const lastSlash = trimmed.lastIndexOf('/');
    if (lastSlash === -1) {
        return { name: stripDelimiters(trimmed), kind: inferKindFromDescriptor(trimmed) };
    }
    const descriptor = trimmed.slice(lastSlash + 1); // e.g. "Foo#bar().", "helper().", "helper().(value)"
    const cleaned = stripDelimiters(descriptor);
    // Parameter-style descriptors: "helper().(value)" → name = "value"
    if (descriptor.includes('.(')) {
        const parenMatch = descriptor.match(/\.\(([^)]+)\)/);
        if (parenMatch) {
            return { name: parenMatch[1], kind: 'Parameter' };
        }
        return { name: cleaned, kind: 'Parameter' };
    }
    // Class-style descriptors: "Foo#" with trailing hash only (after stripping backticks/parens)
    if (descriptor.endsWith('#')) {
        return { name: cleaned, kind: 'Class' };
    }
    // Method-style descriptors: "Foo#bar" (after stripping)
    if (descriptor.includes('#')) {
        const hashIndex = cleaned.indexOf('#');
        const methodPart = cleaned.slice(hashIndex + 1);
        if (!methodPart) {
            // Empty after hash → class
            return { name: cleaned.slice(0, hashIndex), kind: 'Class' };
        }
        return { name: methodPart, kind: 'Method' };
    }
    // Function-style descriptors: "helper" (after stripping)
    return { name: cleaned, kind: 'Function' };
}
export function roleIsDefinition(role) {
    return (role & scip.SymbolRole.Definition) !== 0;
}
export function roleDescription(role) {
    const parts = [];
    if (role & scip.SymbolRole.Definition)
        parts.push('definition');
    if (role & scip.SymbolRole.Import)
        parts.push('import');
    if (role & scip.SymbolRole.WriteAccess)
        parts.push('write');
    if (role & scip.SymbolRole.ReadAccess)
        parts.push('read');
    if (role & scip.SymbolRole.Generated)
        parts.push('generated');
    if (role & scip.SymbolRole.Test)
        parts.push('test');
    if (role & scip.SymbolRole.ForwardDefinition)
        parts.push('forward');
    return parts.join(', ') || 'unspecified';
}
function stripDelimiters(raw) {
    // Remove backticks, parentheses, trailing dot, and parameter parens
    const withoutParens = raw.replace(/`/g, '').replace(/\(.*\)/g, '');
    return withoutParens.replace(/[.#]+$/g, '');
}
function inferKindFromDescriptor(descriptor) {
    if (descriptor.includes('#'))
        return 'Method';
    if (descriptor.includes('.('))
        return 'Parameter';
    if (descriptor.endsWith('.'))
        return 'Function';
    return 'Symbol';
}
//# sourceMappingURL=symbols.js.map