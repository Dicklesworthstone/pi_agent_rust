# Session Tree Navigation

Pi sessions are **trees**, not flat message lists. Every entry has an `id` and optional `parentId`, and the session tracks a current **leaf** pointer (your current position in the tree).

Use `/tree` to navigate the session tree and switch the leaf.

The interactive tree UI described below is in `pi --classic`. In the default
stack, `/tree` prints the numbered branch tips, each with its latest user
message, and `/tree <index|id>` (an id prefix works) switches to that tip.

## `/tree` vs `/fork`

| Feature | `/tree` | `/fork` |
|---------|---------|---------|
| Changes | Leaf pointer in the **same session file** | Creates a **new session file** from a chosen point |
| Best for | Quick branch switching / exploring alternatives | Spinning off a tangent into its own session |
| Summary | Optional branch summarization prompt | No branch summarization prompt |

## Tree UI

The tree UI shows a depth-first list with indentation, and marks the current leaf with `← active`.

### Controls

| Key | Action |
|-----|--------|
| ↑/↓ | Move selection |
| Enter | Select node |
| Escape / Ctrl+C | Cancel |
| Ctrl+U | Toggle “user messages only” (hide assistant/tool entries for a cleaner view) |
| Ctrl+O | Toggle “show all” (include hidden system-ish entries like labels/custom) |

### Selection behavior

Pi has two distinct selection modes depending on what you select:

1) **User message (or custom message)**
   - Leaf is set to the **parent** of the selected node (or `null` if the selected node is the root).
   - The selected text is placed into the editor so you can edit + re-submit, creating a new branch.

2) **Non-user message (assistant/tool/compaction/etc.)**
   - Leaf is set to the **selected node**.
   - The editor remains unchanged/empty; you continue from that point.

## Branch summarization

When switching branches, Pi may offer to summarize the branch you’re leaving. The prompt offers:

1) **No summary**
2) **Summarize**
3) **Summarize with custom prompt** (opens a small input box for extra focus instructions)

### Prompt controls

- **Summary choice**: ↑/↓ to choose, Enter to confirm, Escape/Ctrl+C to cancel.
- **Custom prompt**: type instructions (Backspace to edit), Enter to start summarization, Escape/Ctrl+C to return to the choice list.

### What gets summarized

The summary covers the **abandoned path** from the old leaf back toward the common ancestor with your selected target. Summarization stops early if a **compaction** node is encountered.

### Storage

Summaries are stored as `branch_summary` session entries attached to the **new leaf**, so the model can quickly recover context if you return to that branch later.
