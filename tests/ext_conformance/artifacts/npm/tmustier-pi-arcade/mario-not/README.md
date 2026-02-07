# mario-not (experimental)

Mario-style TUI platformer. This project is experimental and tuning is in flux.

## Run

- Command: `/mario-not`
- Extension entrypoint: `arcade/mario-not/mario-not.ts`

## Controls

- Move: Left/Right arrows, A/D, L
- Jump: Up, Space, H
- Walk toggle: X (run is default)
- Pause: P
- Stop: S
- Quit: Q or Esc

## Dev

Tests:

```bash
node --test arcade/mario-not/tests/*.test.js
npx tsc --noEmit -p arcade/mario-not/tsconfig.json
```

Spec:

- `arcade/mario-not/spec.md`
