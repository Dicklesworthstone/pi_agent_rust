function piBrowserForm(action, request) {
  // Called only on a pinned backend node in an isolated world. Selectors and
  // option strings arrive as CDP arguments, never as executable source.
  const fail = (reason) => { throw new Error(`BROWSER_FORM_${reason}`); };
  const attached = () => {
    if (!(this instanceof Element) || !this.isConnected || this.ownerDocument !== document) {
      fail("STALE_ELEMENT");
    }
  };
  const mutable = () => {
    attached();
    if (this.matches(":disabled")) fail("DISABLED");
    let node = this;
    for (let depth = 0; node; ++depth) {
      if (depth >= 256) fail("ANCESTRY_LIMIT");
      if (node instanceof Element && node.hasAttribute("inert")) fail("INERT");
      node = node.parentNode || (node instanceof ShadowRoot ? node.host : null);
    }
  };
  mutable();
  if (action === "form_checked") {
    if (!(this instanceof HTMLInputElement) || !["checkbox", "radio"].includes(this.type)) {
      fail("NOT_CHECKABLE");
    }
    // Read only. Checkedness changes are performed by one native mouse click
    // in Rust, never a JavaScript click() or a checked-property assignment.
    return { kind: this.type, checked: this.checked, indeterminate: this.indeterminate };
  }
  if (!(this instanceof HTMLSelectElement)) fail("NOT_SELECT");
  if (action !== "form_select" && action !== "form_selection") fail("UNKNOWN_ACTION");

  const maxOptions = 4096;
  const maxSelected = 100;
  const maxText = 64 * 1024;
  let remaining = 256 * 1024;
  const text = (value) => {
    // Check UTF-16 length before allocating an encoded copy of page-owned text.
    if (typeof value !== "string" || value.length > maxText) fail("OPTION_TEXT_LIMIT");
    const bytes = new TextEncoder().encode(value).byteLength;
    if (bytes > maxText || bytes > remaining) fail("OPTION_TEXT_LIMIT");
    remaining -= bytes;
    return value;
  };
  if (this.options.length > maxOptions) fail("OPTION_COUNT_LIMIT");
  const options = Array.from(this.options);
  const facts = options.map((option, index) => ({
    index, value: text(option.value), label: text(option.label),
  }));
  const multiple = this.multiple;
  const snapshot = () => {
    mutable();
    if (this.multiple !== multiple || this.options.length !== options.length) {
      fail("OPTIONS_CHANGED");
    }
    const selected = [];
    let bytes = 0;
    for (let i = 0; i < options.length; ++i) {
      const option = options[i];
      if (this.options[i] !== option || option.value !== facts[i].value || option.label !== facts[i].label) {
        fail("OPTIONS_CHANGED");
      }
      if (option.selected) {
        if (selected.length >= maxSelected) fail("SELECTED_COUNT_LIMIT");
        bytes += new TextEncoder().encode(facts[i].value).byteLength;
        bytes += new TextEncoder().encode(facts[i].label).byteLength;
        if (bytes > maxText) fail("SELECTED_TEXT_LIMIT");
        selected.push(facts[i]);
      }
    }
    return { multiple, selected };
  };
  if (action === "form_selection") return snapshot();
  if (!Array.isArray(request) || request.length > maxSelected) fail("INVALID_OPTIONS");
  if (!multiple && request.length > 1) fail("NOT_MULTIPLE");

  // Resolve the whole selection before the first mutation. In particular, a
  // later missing/ambiguous/disabled option must not leave an earlier one set.
  const wanted = new Set();
  for (const item of request) {
    if (!item || typeof item !== "object" || Array.isArray(item) || Object.keys(item).length !== 1) {
      fail("INVALID_OPTION");
    }
    const key = Object.keys(item)[0];
    if (key === "index") {
      if (!Number.isInteger(item.index) || item.index < 0 || item.index >= options.length) {
        fail("OPTION_NOT_FOUND");
      }
    } else if ((key !== "value" && key !== "label") || typeof item[key] !== "string") {
      fail("INVALID_OPTION");
    }
    const matches = key === "index" ? [item.index] : facts
      .filter((fact) => fact[key] === item[key]).map((fact) => fact.index);
    if (matches.length === 0) fail("OPTION_NOT_FOUND");
    if (matches.length !== 1) fail("AMBIGUOUS_OPTION");
    const index = matches[0];
    if (wanted.has(index)) fail("DUPLICATE_OPTION");
    if (options[index].matches(":disabled")) fail("DISABLED_OPTION");
    wanted.add(index);
  }
  // Bound result metadata before changing selectedness or firing page handlers.
  let resultBytes = 0;
  for (const index of wanted) {
    resultBytes += new TextEncoder().encode(facts[index].value).byteLength;
    resultBytes += new TextEncoder().encode(facts[index].label).byteLength;
    if (resultBytes > maxText) fail("SELECTED_TEXT_LIMIT");
  }
  const changed = options.some((option, index) => option.selected !== wanted.has(index));
  if (changed) {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "selectedIndex").set.call(this, -1);
    const setSelected = Object.getOwnPropertyDescriptor(HTMLOptionElement.prototype, "selected").set;
    for (const index of wanted) setSelected.call(options[index], true);
    this.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
    this.dispatchEvent(new Event("change", { bubbles: true }));
  }
  const result = snapshot();
  if (result.selected.length !== wanted.size || result.selected.some((option) => !wanted.has(option.index))) {
    fail("SELECTION_NOT_RETAINED");
  }
  return { changed, ...result };
}
