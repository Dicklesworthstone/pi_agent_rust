# Rich Python objects in retained eval cells

The Python kernel accepts scientific objects directly, without a notebook
server or a new Rust output protocol. This extends the explicit-MIME behavior
in `eval.md`; explicit `mime=` calls still validate their supplied data directly.

```python
from PIL import Image
picture = Image.new("RGB", (320, 240))
display(picture)

import pandas as pd
table = pd.DataFrame({"measurement": [1.0, 2.0, float("nan")]})
display(table)

import matplotlib.pyplot as plt
figure, axes = plt.subplots(figsize=(6, 4), dpi=100)
axes.plot([0, 1, 2], [0, 1, 0])
display(figure)
```

These libraries are optional and must be installed in the selected interpreter.
Nothing is installed automatically. Plain cells do not import them. Existing
`%pip install --managed` can provision supported wheels explicitly.

## Representations and final results

PNG-compatible Pillow images and Matplotlib `Figure` objects become native PNG
content blocks. Image dimensions are checked before encoding; the in-memory
PNG writer has a byte cap that accounts for base64 and the remaining display
budget. Figure exports use the figure's DPI and bounding box, rather than
potentially enlarging `savefig.dpi` or `savefig.bbox` defaults. Rendering uses Agg;
the original figure and canvas remain available. `MPLBACKEND` defaults to Agg
only when no backend was selected through the environment.

Pandas `DataFrame` and `Series` objects become explicitly labelled text previews:
at most 40 rows and 12 columns, with values limited to 80 characters by the
formatter. Full dimensions remain visible. This is not a structured JSON table,
a complete data export, or HTML rendering. Missing values and index labels use
Pandas' ordinary textual representations. The source object is not modified.

Other top-level objects can implement synchronous `_repr_mimebundle_`,
`_repr_png_`, `_repr_json_`, or `_repr_markdown_` hooks. The bundle hook receives
`include` and `exclude` keyword arguments and may return a dictionary or a
`(dictionary, metadata)` pair. One representation is selected in this order:
PNG, JSON, Markdown, plain text. Unused metadata is not transmitted or executed.
A hook can return `None` to decline; a bundle's explicit JSON null remains null.
Malformed preferred output or a raising hook is an error, not a silent fallback.

An object in the final expression uses the same rich path. Ordinary strings,
numbers, lists, tuples and dictionaries keep REPL `repr()` semantics, and
arbitrary final coroutines are not implicitly awaited. Explicit displays of
JSON containers still reject unsupported nested objects instead of invoking
custom serializers. `display(image, mime="image/png")` requires encoded PNG
bytes/base64; omit `mime` to render the image object.

## Automatic pyplot output

Ordinary Agg-backed pyplot code now produces image output at cell completion:

```python
import matplotlib.pyplot as plt

figure, axes = plt.subplots(figsize=(6, 4), dpi=100)
axes.plot([0, 1, 2], [0, 1, 0])
axes.set_title("No explicit display call required")
plt.show()
```

`plt.show()` is optional. Automatic capture runs after the final expression,
not at the `show()` call, and does not start a GUI loop. Remaining figures
created by that cell are emitted in creation order through the same bounded
display sink. Figures already explicitly displayed or returned as the rich
final result are closed without emitting a duplicate. Closing removes pyplot
registry ownership, not the Python object: a saved `figure` variable can still
be edited and explicitly displayed in a later cell.

Ownership is attached when an Agg figure manager is created, including when
Matplotlib is imported dynamically. No scientific packages are eagerly
imported; the observer delegates the original module loader. Closed-cell and
context-copied worker-thread creation is refused. Unattributed process-global
figures are neither auto-published nor closed. A retained figure modified in
a later cell requires explicit `display(figure)`; it is not silently adopted.
Direct `matplotlib.figure.Figure()` objects without a pyplot manager likewise
use explicit display or the rich final expression. Non-Agg backend selection
is respected and does not enable automatic capture or closure.

At most 32 cell-owned figures may remain open; closing one during the cell
frees its tracking slot. Automatic images share the ordinary display count,
byte and raster bounds. A failed cell still finalizes its owned plots, retaining
the original error. A rendering failure makes an otherwise successful cell
fail, while prior output and healthy sibling figures remain available. Owned
managers are closed even when their render is refused; none are silently
replayed in a later cell. Plot callbacks cannot recursively create figures
during rendering/finalization. Host cancellation may terminate the process
before Python cleanup; kernel/process ownership remains with the host.

## Streaming and ownership

Each accepted object enters the existing sequenced live-display channel and
final collection. Snapshots are detached at the call; later mutations cannot
change them. The Rust host independently validates frames and reconciles live
and final output. Console sequencing, console-only archives and native
`details.jsonOutputs` retain their existing meanings. JSON representations
appear in `jsonOutputs`; table text and PNG bytes do not.

The original 32-display, 2 MiB per-frame and 4 MiB per-cell limits remain in
force. PNG dimensions remain limited to 16,384 per axis and 16,777,216 pixels.
Nested JSON still has its existing depth/node bounds. A failed formatter leaves
prior accepted output available but does not make the cell successful or undo
completed mutations. Closed or foreign-thread display contexts are refused
before any formatter runs; recursive calls to `display` from a formatter are
refused explicitly. Reset, cancellation and session ownership remain host-owned.

Formatters and plotting callbacks are ordinary guest Python code. Their CPU,
intermediate allocations, library caches and filesystem effects are not
sandboxed by output limits. The existing host timeout still applies. Automatic
output is ownership-scoped, not a process-global scan. Matplotlib itself still
has process-global current-figure state; this is not a plotting sandbox.
HTML, SVG, JPEG, Plotly-specific MIME types, animations and interactive widgets
are not added by this change. No full OMP equivalence is claimed.

## Verification

`tests/eval_python_rich.py` launches the actual bundled server, checks live/final
protocol agreement, and decodes real PNG rasters. It covers standard-library
hooks and optional Pillow/Pandas/Matplotlib workflows without downloading data
or installing packages. These are independent Python substrate probes, not a
substitute for Rust-host or frontend acceptance.

`tests/eval_python_rich.rs` exercises the public `EvalTool`: causal live output,
retained results, failure preservation, stale-task refusal and native PNG
content. Run the authoritative project lane with
`dsr quality --tool pi_agent_rust`. The development session ran the Python probes;
DSR was absent (exit 127), so the seven authored Rust tests, compilation, Clippy
and Rust formatting were not executed.
