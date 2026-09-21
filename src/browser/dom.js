function piBrowserElement(action, options) {
  // Called on a resolved DOM node in a CDP isolated world. Page scripts cannot
  // replace these prototypes or intercept selector strings as executable code.
  if (!(this instanceof Element) || !this.isConnected) {
    throw new Error(
      "Element reference is detached or does not identify an element; take a new snapshot",
    );
  }
  // File inputs are commonly hidden behind a styled upload button. Selecting
  // one is explicit, not a synthetic click; visibility is not a precondition.
  if (action === "file_input" || action === "clear_files") {
    if (!(this instanceof HTMLInputElement) || this.type !== "file") {
      throw new Error("Element is not a file input");
    }
    if (this.matches(":disabled") || this.closest("[inert]")) {
      throw new Error("File input is disabled or inert");
    }
    if (action === "clear_files") {
      // An empty DOM.setFileInputFiles list is a no-op on some Chromium
      // versions. Clearing via the native setter has explicit semantics.
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(this, "");
      this.dispatchEvent(new Event("input", { bubbles: true }));
      this.dispatchEvent(new Event("change", { bubbles: true }));
    }
    return {
      multiple: this.multiple,
      directory: this.webkitdirectory,
      files: Array.from(this.files)
        .slice(0, 11)
        .map((file) => ({
          name: file.name,
          size: file.size,
        })),
    };
  }
  const style = getComputedStyle(this);
  const rect = this.getBoundingClientRect();
  const visible =
    rect.width > 0 &&
    rect.height > 0 &&
    style.visibility !== "hidden" &&
    style.visibility !== "collapse" &&
    style.display !== "none" &&
    Number(style.opacity) !== 0;
  if (action === "visible") return visible;
  if (action === "verify_fill") {
    return (this.isContentEditable ? this.textContent : this.value) === options.text;
  }
  if (!visible) throw new Error("Element is not visible");
  if (this.matches(":disabled") || this.closest("[inert]")) {
    throw new Error("Element is disabled or inert");
  }
  if (action === "point") {
    const left = Math.max(0, rect.left);
    const top = Math.max(0, rect.top);
    const right = Math.min(innerWidth, rect.right);
    const bottom = Math.min(innerHeight, rect.bottom);
    if (right <= left || bottom <= top) throw new Error("Element is outside the viewport");
    const x = (left + right) / 2;
    const y = (top + bottom) / 2;
    let hit = document.elementFromPoint(x, y);
    while (hit && hit.shadowRoot) {
      const inner = hit.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === hit) break;
      hit = inner;
    }
    if (!hit || (hit !== this && !this.contains(hit))) {
      throw new Error("Element is covered by another element");
    }
    return { x, y };
  }
  if (action === "focus" || action === "edit") {
    if (action === "edit") {
      const input = this instanceof HTMLInputElement;
      const textarea = this instanceof HTMLTextAreaElement;
      if (
        (!input && !textarea && !this.isContentEditable) ||
        (input &&
          !["text", "search", "url", "tel", "email", "password", "number"].includes(this.type))
      ) {
        throw new Error("Element is not a supported editable text control");
      }
      if (this.readOnly) throw new Error("Element is read-only");
    }
    this.focus();
    if (this.getRootNode().activeElement !== this) {
      throw new Error("Element could not receive focus");
    }
    if (action === "edit" && options.replace) {
      if (this.isContentEditable) {
        const range = document.createRange();
        range.selectNodeContents(this);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      } else if (this instanceof HTMLInputElement && this.type === "number") {
        // Number inputs do not expose text selection. Clear through the
        // native setter, then insert the requested text through CDP.
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(this, "");
        this.dispatchEvent(new Event("input", { bubbles: true }));
      } else {
        this.select();
      }
    }
    return true;
  }
  throw new Error("Unknown element operation");
}
