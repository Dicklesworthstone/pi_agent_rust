function(options) {
    'use strict';
    // All prototypes are from the selected document's isolated world. Return
    // predicates only: never editable values, page text, HTML or DOM objects.
    if (!(this instanceof Element) || this.ownerDocument !== document) {
        throw new Error('BROWSER_WAIT_STALE');
    }
    const wantsText = options.text !== null;
    if (!this.isConnected) {
        return {attached:false, visible:false, textMatches:wantsText ? false : null};
    }
    const parent = node => node.assignedSlot || node.parentElement ||
        (node.getRootNode && node.getRootNode().host) || null;
    const style = getComputedStyle(this);
    const rect = this.getBoundingClientRect();
    let visible = rect.width > 0 && rect.height > 0 && style.display !== 'none' &&
        style.visibility !== 'hidden' && style.visibility !== 'collapse';
    let ancestor = this;
    for (let depth = 0; ancestor; ++depth) {
        if (depth >= 128) throw new Error('BROWSER_WAIT_LIMIT');
        const css = getComputedStyle(ancestor);
        if (css.display === 'none' || Number(css.opacity) === 0) visible = false;
        ancestor = parent(ancestor);
    }
    if (!wantsText) return {attached:true, visible, textMatches:null};
    const normalize = value => value.replace(/\p{White_Space}+/gu, ' ').replace(/^ | $/gu, '');
    const chunks = [];
    const seen = new WeakSet();
    let nodes = 0;
    let units = 0;
    const append = value => {
        units += value.length;
        if (units > 1024 * 1024) throw new Error('BROWSER_WAIT_LIMIT');
        chunks.push(value);
    };
    function visit(node, depth, textVisible) {
        if (++nodes > 20000 || depth > 128) throw new Error('BROWSER_WAIT_LIMIT');
        if (seen.has(node)) return;
        seen.add(node);
        if (node.nodeType === Node.TEXT_NODE) {
            if (textVisible) {
                const value = node.nodeValue || '';
                if (value.length > 1024 * 1024 - units) throw new Error('BROWSER_WAIT_LIMIT');
                // A host with a closed root may retain light-DOM children
                // that are connected but never rendered. Likewise an unused
                // slot fallback is not visible text. Require actual text boxes
                // without opening a closed root or trusting node connectivity.
                const range = document.createRange();
                range.selectNodeContents(node);
                const boxes = range.getClientRects();
                if (boxes.length > 20000) throw new Error('BROWSER_WAIT_LIMIT');
                for (const box of boxes) {
                    if (box.width > 0 && box.height > 0) {
                        append(value);
                        break;
                    }
                }
            }
            return;
        }
        if (!(node instanceof Element) || node.ownerDocument !== document || !node.isConnected) return;
        const tag = node.localName;
        // In particular, input.value and textarea default/live text are not
        // observable through this helper, including password and autofill data.
        if (['script','style','template','noscript','input','textarea'].includes(tag)) return;
        const css = getComputedStyle(node);
        if (css.display === 'none' || Number(css.opacity) === 0) return;
        const ownVisible = css.visibility !== 'hidden' && css.visibility !== 'collapse';
        const block = /^(block|flex|grid|list-item|table|table-row)$/u.test(css.display);
        if (ownVisible && (block || tag === 'br')) append('\n');
        let children;
        if (tag === 'slot') {
            const assigned = node.assignedNodes();
            children = assigned.length ? assigned : node.childNodes;
        } else {
            children = node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes;
        }
        // Closed roots and other frame documents are never traversed.
        for (const child of children) visit(child, depth + 1, ownVisible);
        if (ownVisible && block) append('\n');
    }
    // Text is rendered DOM text, not an input's value or hidden descendants.
    // A hidden anchor cannot provide a positive text-readiness observation.
    if (visible) visit(this, 0, true);
    const actual = normalize(chunks.join(''));
    const textMatches = visible && (options.exact ? actual === options.text : actual.includes(options.text));
    return {attached:true, visible, textMatches};
}
