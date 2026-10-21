# Browser scripting inside Eval

Both Python and JavaScript kernels install a `browser` prelude over the existing native
`browser` tool. It lets a cell inspect a page, compute a decision, and perform
several actions without another model round trip. Enable the browser tool on
the owning Agent; this helper does not enable it, launch a second driver, or
bypass native approvals, plan restrictions, domain policy, or session ownership.
Standalone Eval retains its read-only bridge and refuses browser operations.

```javascript
globalThis.page = await browser.open({
  name: 'app', url: 'https://example.com', timeout_ms: 10000
});
await page.getByLabel('Email').fill('person@example.test');
await page.getByRole('button', 'Continue').click({
  wait_after: {selector: '#status', text: 'Ready', text_match: 'exact'}
});
display((await page.snapshot()).details);
```

`browser.open` returns an immutable tab handle after successful native
navigation. Its `opened` property retains the complete native result.
`browser.tab(name)` creates an address for an existing named tab or explicit
target ID without a browser request. `browser.tabs()`, `status()`, `start()` and
`stop()` call the corresponding native lifecycle operations. A status query
never implicitly starts Chromium. Stop cannot terminate a browser Pi does not own.

## Tab, frame and locator handles

A tab supports `goto`, `back`, `forward`, `reload`, `close`, `snapshot`,
`accessibility`, `frames`, `screenshot`, `scroll`, `press`, and `evaluate`.
`tab.call(action, args)` exposes other native actions without losing explicit
tab/frame routing. Methods return the full structured tool result, preserving
`content`, `details`, and image blocks rather than guessing a text-only value.
Display an image explicitly using its returned data and MIME type when needed.

`tab.locator(selector)` and `tab.ref(selector)` retain an exact native selector:
CSS, semantic selectors, or an existing `@e...` reference. No query runs until
an operation is invoked. Locators support `click`, `hover`, `fill`, `type`,
`press`, `waitFor`, `upload`, `download`, and `dragTo`.

`getByText`, `getByLabel`, and `getByTestId` construct the native semantic forms.
`getByRole(role, name, exact = true)` quotes the accessible name as literal data.
Drag destinations must be locators from the same tab and frame; native gesture
restrictions still apply. Fill accepts an empty string but never coerces a
number or null into text. All native postconditions/options remain explicit.

```javascript
const frames = await page.frames();
display(frames.details);
// Use an actual ID returned by native frame discovery, not a CSS selector.
const embedded = page.frame('RETURNED_FRAME_ID');
await embedded.getByLabel('Search').fill('structured concurrency');
await embedded.getByLabel('Search').press('Enter');
```

Frame selection stays on that handle. It cannot be overwritten in options or
fall back to the main document. Native actions unsupported for frames refuse.
Handles retain addresses and selectors, not permission or an immutable browser
connection. Reusing a tab alias after another caller closes/reopens it can
address the replacement tab. Native snapshot references remain document-bound
and fail when stale; the helper never refreshes or retries them automatically.

## One native batch for predetermined sequences

`tab.batch(steps, {timeout_ms})` sends 1–16 steps through the existing native
batch engine. Each step has `call` plus optional `expect` and `bind`. The helper
adds its tab/frame to every call and refuses conflicting scope arguments.

```javascript
await page.batch([
  {call: {action: 'snapshot'},
   expect: {pointer: '/snapshot/elements/0/text', equals: 'Move card'}},
  {call: {action: 'drag', target_selector: 'testid/drop-zone'},
   bind: {selector: {step: 0, pointer: '/snapshot/elements/0/ref_id'}}},
  {call: {action: 'snapshot'}}
], {timeout_ms: 15000});
```

The complete helper argument shape is checked before the batch is sent. Native
validation then controls bindings, assertions, action availability and failure
semantics. This uses the ordinary conversation execution lease and original
batch deadline; it does not emulate a batch as many separate helper calls.
For dynamic decisions, use normal JavaScript between awaited tab operations.
Separate calls do not acquire a graph-wide lease or atomic transaction.

## Failure, ownership and data boundaries

Unsuccessful native results throw `BrowserActionError`, carrying the complete
native result in `error.result` and the action in `error.action`. This preserves
completed input and batch-step evidence when a later postcondition fails.
Transport/host refusals also raise. No action is automatically replayed and
already-completed effects are not rolled back. Catching an error deliberately
allows the script to continue; native call receipts still record the failure.

Every method re-enters the current invocation's native host. Retaining a tab or
locator across cells cannot preserve a previous approval. The helper retains
only the ordinary JS bridge function, never a Rust Agent callback or session
credential. Calls share the cell's existing 64 host-call allowance and deadline;
`Promise.all` cannot override native serialization or create another allowance.

Options cannot replace explicit actions, tabs, frames, URLs or supplied input
values. Arguments must be JSON data: non-finite numbers, undefined, functions,
getters, sparse arrays, custom prototypes and symbol keys are refused before
native dispatch. Data is copied without running `toJSON` hooks. The helper caps
shape at depth 32 and 16,384 values and charges 256 KiB of key/string code units;
the native encoded-request limit remains authoritative. This is argument
validation, not a security sandbox against arbitrary code in an Eval cell.

`evaluate` requires an explicit page-source string. Functions and closures are
not serialized, and there is no `browser.run` or Playwright compatibility claim.
The two language interfaces share that native policy path; direct
`await tool.result('browser', {...})` remains available in either language.

## Python workflows

Python uses keyword options and snake_case method names. `browser.tab`,
`frame`, `locator`, `ref` and the `get_by_*` selectors construct addresses
synchronously; browser actions are asynchronous and must be awaited.

```python
page = await browser.open(name='app', url='https://example.com', timeout_ms=10000)
await page.get_by_label('Email').fill('person@example.test')
await page.get_by_role('button', 'Continue').click(
    wait_after={'selector': '#status', 'text': 'Ready', 'text_match': 'exact'},
)
observed = await page.snapshot()
display(observed['details'])
```

Python exposes the same tab actions as JavaScript, including `call(action,
**options)`, explicit-source `evaluate`, screenshots, frame discovery and
navigation. Its locator methods are `click`, `hover`, `fill`, `type`, `press`,
`wait_for`, `upload`, `download` and `drag_to`. Semantic constructors are
`get_by_text`, `get_by_label`, `get_by_test_id` and `get_by_role(role, name,
exact=True)`. `page.frame(actual_frame_id)` pins each subsequent operation's
frame argument; it does not change a global browser selection.

Use ordinary Python to choose from returned evidence, then send one native
batch for a predetermined sequence. No additional model round trip is required:

```python
seen = await page.snapshot()
save = next((element for element in seen['details']['snapshot']['elements']
             if element['text'] == 'Save'), None)
if save is None:
    raise RuntimeError('Save control is missing')
try:
    await page.ref(save['ref_id']).click()
except browser.ActionError as error:
    display(error.result)  # Includes native partial-step/postcondition evidence.
    raise
```

`await page.batch(steps, timeout_ms=15000)` sends one native batch with the
same `call`, `bind` and `expect` dictionaries shown above. Its complete shape
is checked before dispatch; an invalid later step cannot execute a valid prefix.
Python handles retain only tab/frame/selector addresses and the ordinary guest
transport. Reusing them in later cells re-enters current host policy. Explicit
kernel reset recreates the helper and discards old user variables. It does not
close an independently retained native browser session.

Python accepts only exact built-in JSON types: dictionaries with string keys,
lists, strings, booleans, null, finite floats and integers from -2^63 through
2^64-1. It does not invoke custom iterators/serializers or coerce tuples, objects,
numeric field values or huge integers. It bounds shape to depth 32 and 16,384
values and both string/key UTF-8 bytes and encoded arguments to 256 KiB; the
native wire limit remains authoritative, including transport framing. Invalid
Unicode is refused. Arguments are copied when the coroutine executes, before
waiting for the native reply; as with ordinary Python coroutines, creating a
coroutine without awaiting/scheduling it performs no action or snapshot.

The compiled-in Python helper and kernel are staged in the existing private
kernel directory. A small bootstrap loads the kernel by its exact staged path,
installs the helper before its receive loop, and retains `-c` workspace import
semantics and the prior argument shape. It does not edit the kernel source,
start a second process, add the private directory to `sys.path`, or create an
alternative timeout/bridge. The user namespace remains `__name__ == '__main__'`.

## Verification

The production prelude has a dependency-free controlled-transport suite:
`node --test tests/eval_browser_prelude.test.cjs`. Native QuickJS/host integration
coverage is in `tests/eval_browser_prelude.rs`. Neither suite implies real-CDP,
Chromium or live-site acceptance. `dsr quality --tool pi_agent_rust` remains the
authoritative Rust quality command; it was unavailable during implementation.

Python's production helper and staged bootstrap are exercised by
`python3 tests/eval_browser_prelude_test.py` with controlled native replies.
That suite passed 25 cases; the unchanged JavaScript suite passed 20. Six added
Rust regressions cover the actual Python kernel/host path, retained state,
workspace imports, reset, current policy and partial errors. They were authored,
not compiled or executed: DSR was unavailable (exit 127). Controlled-transport
checks are not a substitute for native Rust or real-browser acceptance.
