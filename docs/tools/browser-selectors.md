# Semantic browser selectors

The native `browser` tool accepts semantic selectors in its existing `selector`
field. No new browser, shell helper, or eval runtime is introduced. Resolution
produces the same document-owned backend node ID used by CSS and snapshot `@eN`
references; the existing native input, frame checks and tool authority still apply.

```json
{"action":"click","tab":"settings","selector":"role/button[name=\"Save changes\" exact]"}
{"action":"fill","tab":"settings","selector":"label/Display name","text":"Example"}
{"action":"set_checked","tab":"settings","selector":"label/Show line numbers","checked":true}
{"action":"select_option","tab":"settings","selector":"label/Theme","options":[{"value":"dark"}]}
```

Normal host approval and browser/domain policies remain in force.

## Supported selectors

| Prefix | Matches |
| --- | --- |
| `text/Save changes` | Smallest visible rendered element containing the text. |
| `role/button[name="Save" exact]` | Chromium-computed role and exact accessible name. Omit `exact` for substring names, or omit the filter to select a unique role. |
| `label/Email address` | Chromium accessible name of a form-control role: textbox, searchbox, combobox, listbox, checkbox, radio, switch, slider, spinbutton, or button. Includes associated labels and ARIA names. |
| `placeholder/Email` | Literal `placeholder` attribute containing the text. |
| `testid/save-button` | Exact `data-testid` attribute, with spaces preserved. |
| `alt/Company logo` | Literal `alt` attribute containing the text. |
| `title/Explanation` | Literal `title` attribute containing the text. |

Matching is case-sensitive. Text-like queries and names collapse Unicode
whitespace; test IDs remain exact. Role filter values must be double-quoted JSON
strings, so quotes and Unicode escapes are unambiguous. All other prefixes take
the remainder literally, including quotes and slashes; they are never JavaScript
or regular expressions. No new semantics are assigned to ordinary CSS or `@eN`.

A semantic action must have exactly one matching element. Multiple matches are an
error before input, not permission to select the first one. Narrow the accessible
name or use a fresh snapshot reference. `wait_for` repeats a no-match query within
the existing whole-operation deadline; an ambiguous query is an immediate error.

Semantic selectors work with `click`, `type`, `fill`, `press`, `wait_for`,
`select_option`, `set_checked`, `upload`, and frame-scoped `scroll`. They are
explicitly rejected on actions that would ignore the selector, including ordinary
viewport scrolling, inspection, exports and downloads. They never produce mock
success; explicit mock mode refuses them.

## Documents, shadow DOM and limits

An explicit `frame` searches only that frame and requires the existing explicit
`tab`. No match never falls back to the parent document. Role/label matching uses
Chromium's accessibility tree; DOM text and attribute lookup traverses open shadow
roots and assigned slots, but never enters iframe documents or closed shadow roots.
Lookup does not bypass the existing hit testing, disabled-control checks or frame
coordinate projection. A document change during lookup invalidates the result.
No semantic element handle is retained across calls: each call resolves afresh.

Input selectors are limited to 16 KiB; stricter existing action limits still
apply, including the upload action's 4 KiB selector limit. DOM lookup visits at most 20,000 nodes with
128 levels of nesting and 4 Mi UTF-16 code units of text work. Accessible-name
lookup checks at most 20,000 AX nodes and 128 candidate IDs. Exceeding a bound
fails explicitly rather than selecting from a knowingly partial search.
Text lookup excludes script/style/template content, hidden subtrees and editable
input/textarea values. Role and label matching never consult the AX `value` field.
This avoids copying field values merely to locate an element; it is not a promise
that a site's labels, titles, or other visible text cannot themselves contain secrets.

This implements seven useful OMP-style selector families, not full Puppeteer or
Playwright compatibility. XPath, `aria/`, explicit `pierce/`, regular-expression
queries, full accessible-name emulation in JavaScript, and Playwright-only CSS
pseudos are not added.

## Verification

Parser, preflight, accessibility selection and redaction regressions live beside
the resolver. `tests/browser_semantic_selectors.rs` exercises public dispatch and
contains an explicitly ignored live-Chromium workflow for the DSR browser lane.
The implementation-session Chromium probe executes the exact embedded DOM helper
and an independent CDP path, not the Rust host. Rust compilation, tests, formatting
and Clippy require `dsr quality --tool pi_agent_rust`; no direct Cargo or Actions
result substitutes for that gate.
