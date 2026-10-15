function(locator) {
    'use strict';
    // Executed in the selected document's isolated world. Values are arguments,
    // never source text. This helper only finds a node; it never sends input.
    const MAX_NODES = 20000;
    const MAX_DEPTH = 128;
    const MAX_TEXT = 4 * 1024 * 1024;
    const kind = locator.kind;
    if (!['text', 'placeholder', 'testid', 'alt', 'title'].includes(kind)) {
        throw new Error('BROWSER_SELECTOR_QUERY_FAILED');
    }
    // Unicode White_Space matches Rust's split_whitespace, including U+0085
    // and excluding BOM. Keep parsed and DOM-side matching consistent.
    const normalize = value => value.replace(/\p{White_Space}+/gu, ' ').replace(/^ | $/gu, '');
    const wanted = locator.value;
    let nodes = 0;
    let textWork = 0;
    const seen = new WeakSet();
    const matches = new Set();
    const spend = value => {
        textWork += value.length;
        if (textWork > MAX_TEXT) throw new Error('BROWSER_SELECTOR_LIMIT');
        return value;
    };
    const add = node => {
        matches.add(node);
        if (matches.size > 1) throw new Error('BROWSER_SELECTOR_AMBIGUOUS');
    };
    const attribute = {
        placeholder: 'placeholder', testid: 'data-testid', alt: 'alt', title: 'title'
    }[kind];
    function visit(node, depth) {
        if (++nodes > MAX_NODES || depth > MAX_DEPTH) {
            throw new Error('BROWSER_SELECTOR_LIMIT');
        }
        if (seen.has(node)) return { text: '', found: false };
        seen.add(node);
        if (node.nodeType === Node.TEXT_NODE) {
            return { text: kind === 'text' ? spend(node.nodeValue || '') : '', found: false };
        }
        if (node.nodeType !== Node.ELEMENT_NODE || node.ownerDocument !== document || !node.isConnected) {
            return { text: '', found: false };
        }
        const tag = node.localName;
        if (['script', 'style', 'template', 'noscript'].includes(tag)) {
            return { text: '', found: false };
        }
        const style = getComputedStyle(node);
        if (style.display === 'none' || node.hidden || node.getAttribute('aria-hidden') === 'true') {
            return { text: '', found: false };
        }
        // visibility may be restored by a descendant. Still visit children,
        // but do not admit this element or its direct text while it is hidden.
        const visible = style.visibility !== 'hidden' && style.visibility !== 'collapse';
        if (attribute && visible && node.getClientRects().length !== 0) {
            const value = node.getAttribute(attribute);
            if (value !== null) {
                spend(value);
                const actual = kind === 'testid' ? value : normalize(value);
                if (kind === 'testid' ? actual === wanted : actual.includes(wanted)) add(node);
            }
        }
        let text = tag === 'br' && visible ? '\n' : '';
        let descendantFound = false;
        // Follow the rendered open-shadow/slot tree, never enter frame documents.
        // Passwords and editable values are not read as a text-query side effect.
        if (tag !== 'textarea' && tag !== 'input') {
            let children;
            if (tag === 'slot') {
                const assigned = node.assignedNodes();
                children = assigned.length ? assigned : node.childNodes;
            } else {
                children = node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes;
            }
            for (const child of children) {
                if (!visible && child.nodeType === Node.TEXT_NODE) continue;
                const result = visit(child, depth + 1);
                descendantFound ||= result.found;
                if (kind === 'text') text += spend(result.text);
            }
        }
        let found = descendantFound;
        if (kind === 'text' && visible && !descendantFound && node.getClientRects().length !== 0) {
            const actual = normalize(spend(text));
            if (actual.includes(wanted)) {
                add(node);
                found = true;
            }
        }
        if (kind === 'text' && /^(block|flex|grid|list-item|table|table-row)$/u.test(style.display)) {
            text += '\n';
        }
        return { text, found };
    }
    if (document.documentElement) visit(document.documentElement, 0);
    return matches.size === 1 ? matches.values().next().value : null;
}
