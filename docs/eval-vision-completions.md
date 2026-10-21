# Image-aware auxiliary completions

The native `__pi_complete` eval operation now accepts optional `images`, using
exactly the same model-capability check, image validation and policy-controlled
reader as `__pi_judge` and `__pi_judge_batch`. This adds one-shot visual answers
and schema-validated extraction without another executor or provider binding.

```javascript
const result = JSON.parse(await tool.call('__pi_complete', {
  prompt: 'Extract the component names visible in this diagram.',
  images: [{type: 'image', mimeType: 'image/png', data: screenshotBase64}],
  schema: {
    type: 'object', required: ['components'], additionalProperties: false,
    properties: {components: {type: 'array', items: {type: 'string'}}}
  }
}));
display(result.data);
```

Python uses the same payload with `json.loads(await tool.call('__pi_complete',
request))`. An Agent-owned cell may also provide a local path, for example
`images: ['diagram.png']`. This enters the current Agent's enabled `read` tool
with the captured session identity and existing approval/path policy. Standalone
EvalTool instances must use embedded images; a local fallback reader is not
granted. No URL fetching or implicit network image download is added.

Requests still use the explicit per-cell `judge_model`, or the SDK host's bound
Judge. The selected catalog model must declare image input; SDK hosts explicitly
use `Judge::new(provider, options).with_image_support(true)`. Unsupported bindings
fail before reading image paths or calling the model, rather than dropping images
or selecting a fallback. No request field can grant image support.

For direct Rust embedding, first parse the text/schema request and then call
`CompletionRequest::with_images(Vec<ImageContent>)`. Typed images are validated
before being retained. The textual JSON parser deliberately does not load files.

Up to four ordered images use the current native judgment limits: 4,718,592
decoded bytes per image and 8 MiB of encoded image payload per request. PNG,
JPEG, GIF and WebP envelopes are validated. Existing kernel/bridge limits also
apply. There is no new raster decode, resize, OCR or visual-secret detector.
Text privacy projection covers prompt, system and schema, not the pixels; review
images before sending. Read notes and image paths are not appended to the prompt.

All descriptors and the schema are validated before host reads or attempt
reservation. Model support and the whole read count are checked before the first
read. Every requested image must be acquired successfully before generation;
a denied or malformed later image sends no partial request. Reads have ordinary
native tool receipts. Model completions share the existing 16-attempt allowance,
original cell owner/deadline and 60-second auxiliary request ceiling. No task is
detached and cancellation never starts a replacement request.

Text-only calls remain unchanged. Image calls produce native image blocks after
the screened text, plus `imageCount` on a successful result. The existing
`pi.completion.v1` terminal text, provider identity and usage fields remain. With
`schema`, all existing strict JSON validation still applies: duplicate keys,
unknown fields, invalid data, truncated responses and tool requests fail without
repair or retry. Validated structure is not a factual-accuracy guarantee.

Payload-free `PI_COMPLETION_IMAGE_*` errors distinguish input/limit problems,
unsupported bindings, missing host authority, read failure and read-call limits.
Failed admitted attempts remain visible with unknown usage, not a zero bill.
Image bytes, paths and credential values are not copied into attempt receipts.

Eight added native admission/controlled-provider tests cover image extraction,
preflight, capability refusal, policy-controlled reads, text privacy, late-read
failure, schema refusal, owner cancellation, stream disposal and both public
Python/QuickJS bridges. These tests were authored, not run in this environment.
The required `dsr quality --tool pi_agent_rust` failed to start because `dsr` is
absent (exit 127). No Rust compilation, formatting, Clippy, live-model, visual
accuracy or cross-platform acceptance is claimed. Existing judgment image and
completion tests remain intact.
