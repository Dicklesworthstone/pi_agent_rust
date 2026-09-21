/**
 * TS Conformance Harness: load a pi-mono extension with a mock runtime and
 * emit a deterministic JSON snapshot + captured hostcall invocations.
 *
 * Usage (from repo root):
 *   bun run tests/ext_conformance/ts_harness/run_extension.ts <extension-path> <mock-spec-path> [cwd]
 *
 * Optional env:
 * - PI_TS_CAPTURE_LOGS=1  Capture console output from extensions into JSON output (suppresses stdout noise).
 *
 * Notes:
 * - This harness uses pi-mono's loader from the compiled dist/ output.
 * - It injects runtime action mocks (sendMessage, setModel, etc).
 * - It replaces global fetch with a mock based on the mock spec.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

interface EventFire {
  event?: string;
  payload?: JsonValue;
  expected_response?: JsonValue;
}

interface MockSpec {
  schema?: string;
  extension_id?: string;
  description?: string;
  session?: {
    name?: string;
    file?: string;
    state?: JsonValue;
    messages?: JsonValue[];
    entries?: JsonValue[];
    branch?: JsonValue[];
    accept_mutations?: boolean;
  };
  http?: {
    rules?: HttpRule[];
    default_response?: HttpResponse;
  };
  exec?: {
    rules?: ExecRule[];
    default_result?: ExecResult;
  };
  tools?: {
    active_tools?: string[];
    all_tools?: Array<{ name: string; description?: string }>;
    invocations?: JsonValue[];
  };
  ui?: {
    capture?: boolean;
    responses?: Record<string, JsonValue>;
    confirm_default?: boolean;
    dialog_default?: string;
  };
  events?: {
    fire_sequence?: EventFire[];
  };
  model?: {
    current?: { provider?: string; model_id?: string; name?: string };
    thinking_level?: string;
    available_models?: JsonValue[];
    accept_mutations?: boolean;
  };
}

interface ExecRule {
  command: string;
  args?: string[];
  result: ExecResult;
}

interface ExecResult {
  stdout: string;
  stderr: string;
  code: number;
  killed?: boolean;
}

interface HttpRule {
  method: string;
  url: string;
  response: HttpResponse;
}

interface HttpResponse {
  status: number;
  headers?: Record<string, string>;
  body?: string;
}

interface EventCapture {
  event: string;
  payload: JsonValue;
  handler_count: number;
  responses: JsonValue[];
  response: JsonValue;
  expected_response?: JsonValue;
  expected_match?: boolean;
  error?: string;
}

interface CaptureLog {
  sendMessage: Array<{ message: JsonValue; options?: JsonValue }>;
  sendUserMessage: Array<{ content: JsonValue; options?: JsonValue }>;
  appendEntry: Array<{ customType: string; data?: JsonValue }>;
  setSessionName: Array<{ name: string }>;
  setLabel: Array<{ entryId: string; label?: string }>;
  setActiveTools: Array<{ tools: string[] }>;
  setModel: Array<{ model: JsonValue }>;
  setThinkingLevel: Array<{ level: string }>;
  exec: Array<{ command: string; args: string[]; cwd: string; matched: boolean } & ExecResult>;
  http: Array<{ method: string; url: string; matched: boolean; response: HttpResponse }>;
  ui: Array<{ op: string; payload?: JsonValue; result?: JsonValue }>;
  events: EventCapture[];
  warnings: string[];
}

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const PI_MONO_ROOT = path.resolve(__dirname, "../../../legacy_pi_mono_code/pi-mono");

const loaderPath = path.join(PI_MONO_ROOT, "packages/coding-agent/dist/core/extensions/loader.js");

const CAPTURE_LOGS = process.env.PI_TS_CAPTURE_LOGS === "1";
const FORCE_EXIT = process.env.PI_TS_FORCE_EXIT !== "0";
const capturedLogs: Array<{ level: "log" | "warn" | "error"; message: string }> = [];
const originalConsole = {
  log: console.log.bind(console),
  warn: console.warn.bind(console),
  error: console.error.bind(console),
};

function applyDeterministicGlobals() {
  const timeRaw = process.env.PI_DETERMINISTIC_TIME_MS;
  const stepRaw = process.env.PI_DETERMINISTIC_TIME_STEP_MS;
  if (timeRaw && timeRaw.trim().length > 0) {
    const base = Number(timeRaw);
    if (Number.isFinite(base)) {
      const stepValue = stepRaw ? Number(stepRaw) : 1;
      const step = Number.isFinite(stepValue) ? stepValue : 1;
      let tick = 0;
      const nextNow = () => {
        const value = base + step * tick;
        tick += 1;
        return value;
      };
      const OriginalDate = Date;
      class DeterministicDate extends OriginalDate {
        constructor(...args: any[]) {
          if (args.length === 0) {
            super(nextNow());
          } else {
            super(...args);
          }
        }
        static now() {
          return nextNow();
        }
      }
      DeterministicDate.UTC = OriginalDate.UTC;
      DeterministicDate.parse = OriginalDate.parse;
      (globalThis as any).Date = DeterministicDate;
    }
  }

  const randRaw = process.env.PI_DETERMINISTIC_RANDOM;
  const randSeedRaw = process.env.PI_DETERMINISTIC_RANDOM_SEED;
  if (randRaw && randRaw.trim().length > 0) {
    const value = Number(randRaw);
    if (Number.isFinite(value)) {
      Math.random = () => value;
    }
  } else if (randSeedRaw && randSeedRaw.trim().length > 0) {
    let state = Number(randSeedRaw);
    if (Number.isFinite(state)) {
      state = state >>> 0;
      Math.random = () => {
        state = (state * 1664525 + 1013904223) >>> 0;
        return state / 4294967296;
      };
    }
  }

  const detCwd = process.env.PI_DETERMINISTIC_CWD;
  if (detCwd && detCwd.trim().length > 0) {
    try {
      Object.defineProperty(process, "cwd", {
        value: () => detCwd,
        configurable: true,
      });
    } catch {}
  }

  const detHome = process.env.PI_DETERMINISTIC_HOME;
  if (detHome && detHome.trim().length > 0) {
    try {
      process.env.HOME = detHome;
      process.env.USERPROFILE = detHome;
    } catch {}
  }
}

function serializeArgs(args: unknown[]): string {
  return args
    .map((arg) => {
      if (typeof arg === "string") return arg;
      try {
        return JSON.stringify(arg);
      } catch {
        return String(arg);
      }
    })
    .join(" ");
}

if (CAPTURE_LOGS) {
  console.log = (...args: unknown[]) => {
    capturedLogs.push({ level: "log", message: serializeArgs(args) });
  };
  console.warn = (...args: unknown[]) => {
    capturedLogs.push({ level: "warn", message: serializeArgs(args) });
  };
  console.error = (...args: unknown[]) => {
    capturedLogs.push({ level: "error", message: serializeArgs(args) });
  };
}

function readJson(filePath: string): JsonValue {
  const raw = fs.readFileSync(filePath, "utf-8");
  return JSON.parse(raw) as JsonValue;
}

function normalizeMockSpec(raw: JsonValue): MockSpec {
  if (!raw || typeof raw !== "object") return {};
  return raw as MockSpec;
}

function toJsonValue(value: unknown): JsonValue {
  if (value === undefined) return null;
  try {
    const serialized = JSON.stringify(value);
    if (serialized === undefined) return null;
    return JSON.parse(serialized) as JsonValue;
  } catch {
    return String(value);
  }
}

function jsonEquals(left: unknown, right: unknown): boolean {
  return JSON.stringify(toJsonValue(left)) === JSON.stringify(toJsonValue(right));
}

function pickExecRule(
  rules: ExecRule[] | undefined,
  command: string,
  args: string[],
): ExecRule | undefined {
  if (!rules) return undefined;
  return rules.find((rule) => {
    if (rule.command !== command) return false;
    if (!rule.args) return true;
    if (rule.args.length !== args.length) return false;
    return rule.args.every((val, idx) => val === args[idx]);
  });
}

function pickHttpRule(
  rules: HttpRule[] | undefined,
  method: string,
  url: string,
): HttpRule | undefined {
  if (!rules) return undefined;
  return rules.find(
    (rule) => rule.method.toUpperCase() === method.toUpperCase() && rule.url === url,
  );
}

function createFallbackRuntime() {
  const notInitialized = () => {
    throw new Error(
      "Extension runtime not initialized. Action methods cannot be called during extension loading.",
    );
  };
  return {
    sendMessage: notInitialized,
    sendUserMessage: notInitialized,
    appendEntry: notInitialized,
    setSessionName: notInitialized,
    getSessionName: notInitialized,
    setLabel: notInitialized,
    getActiveTools: notInitialized,
    getAllTools: notInitialized,
    setActiveTools: notInitialized,
    setModel: () => Promise.reject(new Error("Extension runtime not initialized")),
    getThinkingLevel: notInitialized,
    setThinkingLevel: notInitialized,
    exec: async () => {
      throw new Error("Extension runtime not initialized");
    },
    flagValues: new Map<string, boolean | string>(),
    pendingProviderRegistrations: [] as Array<{
      name: string;
      config: { models?: Array<{ id?: string; name?: string }> };
    }>,
  };
}

function createFallbackExtension(extensionPath: string, resolvedPath: string) {
  return {
    path: extensionPath,
    resolvedPath,
    handlers: new Map<
      string,
      Array<(payload: JsonValue, ctx: any) => Promise<unknown> | unknown>
    >(),
    tools: new Map<string, { definition: JsonValue; extensionPath: string }>(),
    messageRenderers: new Map<string, unknown>(),
    commands: new Map<string, JsonValue>(),
    flags: new Map<string, JsonValue>(),
    shortcuts: new Map<string, JsonValue>(),
  };
}

function createFallbackApi(
  extension: ReturnType<typeof createFallbackExtension>,
  runtime: ReturnType<typeof createFallbackRuntime>,
  cwd: string,
) {
  return {
    on(event: string, handler: (payload: JsonValue, ctx: any) => Promise<unknown> | unknown) {
      const list = extension.handlers.get(event) ?? [];
      list.push(handler);
      extension.handlers.set(event, list);
    },
    registerTool(tool: any) {
      extension.tools.set(tool.name, {
        definition: tool,
        extensionPath: extension.path,
      });
    },
    registerCommand(name: string, options: JsonValue) {
      extension.commands.set(name, { name, ...(options as any) });
    },
    registerShortcut(shortcut: string, options: JsonValue) {
      extension.shortcuts.set(shortcut, {
        shortcut,
        extensionPath: extension.path,
        ...(options as any),
      });
    },
    registerFlag(name: string, options: any) {
      extension.flags.set(name, { name, extensionPath: extension.path, ...options });
      if (options?.default !== undefined) {
        runtime.flagValues.set(name, options.default);
      }
    },
    registerMessageRenderer(customType: string, renderer: unknown) {
      extension.messageRenderers.set(customType, renderer);
    },
    getFlag(name: string) {
      if (!extension.flags.has(name)) return undefined;
      return runtime.flagValues.get(name);
    },
    sendMessage(message: JsonValue, options?: JsonValue) {
      runtime.sendMessage(message, options);
    },
    sendUserMessage(content: JsonValue, options?: JsonValue) {
      runtime.sendUserMessage(content, options);
    },
    appendEntry(customType: string, data?: JsonValue) {
      runtime.appendEntry(customType, data);
    },
    setSessionName(name: string) {
      runtime.setSessionName(name);
    },
    getSessionName() {
      return runtime.getSessionName();
    },
    setLabel(entryId: string, label?: string) {
      runtime.setLabel(entryId, label);
    },
    exec(command: string, args: string[], options?: { cwd?: string }) {
      return runtime.exec(command, args, options?.cwd ?? cwd);
    },
    getActiveTools() {
      return runtime.getActiveTools();
    },
    getAllTools() {
      return runtime.getAllTools();
    },
    setActiveTools(toolNames: string[]) {
      runtime.setActiveTools(toolNames);
    },
    setModel(model: JsonValue) {
      return runtime.setModel(model);
    },
    getThinkingLevel() {
      return runtime.getThinkingLevel();
    },
    setThinkingLevel(level: string) {
      runtime.setThinkingLevel(level);
    },
    registerProvider(name: string, config: { models?: Array<{ id?: string; name?: string }> }) {
      runtime.pendingProviderRegistrations.push({ name, config });
    },
    events: {
      on() {},
      emit() {},
    },
  };
}

async function fallbackLoadExtensions(paths: string[], cwd: string) {
  const extensions = [];
  const errors = [];
  const runtime = createFallbackRuntime();
  for (const extensionPath of paths) {
    const resolvedPath = path.resolve(cwd, extensionPath);
    try {
      const imported = await import(pathToFileURL(resolvedPath).href);
      const factory = imported.default ?? imported;
      if (typeof factory !== "function") {
        errors.push({
          path: extensionPath,
          error: `Extension does not export a valid factory function: ${extensionPath}`,
        });
        continue;
      }
      const extension = createFallbackExtension(extensionPath, resolvedPath);
      await factory(createFallbackApi(extension, runtime, cwd));
      extensions.push(extension);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      errors.push({ path: extensionPath, error: `Failed to load extension: ${message}` });
    }
  }
  return { extensions, errors, runtime };
}

async function resolveLoadExtensions() {
  try {
    const imported = await import(loaderPath);
    return imported.loadExtensions as (paths: string[], cwd: string) => Promise<any>;
  } catch {
    return fallbackLoadExtensions;
  }
}

function shouldRetryWithFallbackLoader(result: any): boolean {
  const errors = result?.errors;
  if (!Array.isArray(errors) || errors.length === 0) return false;
  return errors.every((entry) => {
    const message = String(entry?.error ?? "");
    return (
      message.includes("Cannot find module '@mariozechner/pi-ai'") ||
      message.includes("Cannot find module '@mariozechner/pi-tui'") ||
      message.includes("Cannot find module '@mariozechner/pi-agent-core'") ||
      message.includes("Cannot find module '@sinclair/typebox'")
    );
  });
}

function installFetchMock(spec: MockSpec, capture: CaptureLog): () => void {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const url =
      typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    const method = (
      init?.method ?? (typeof input === "object" && "method" in input ? input.method : "GET")
    ).toUpperCase();
    const match = pickHttpRule(spec.http?.rules, method, url);
    const response = match?.response ??
      spec.http?.default_response ?? { status: 404, body: "mock: no HTTP rule matched" };
    capture.http.push({ method, url, matched: Boolean(match), response });
    return new Response(response.body ?? "", {
      status: response.status,
      headers: response.headers ?? {},
    });
  };
  return () => {
    globalThis.fetch = originalFetch;
  };
}

function buildEventContext(spec: MockSpec, capture: CaptureLog, cwd: string): any {
  const choose = (value: unknown, fallback: unknown) => (value === undefined ? fallback : value);
  const uiResponses = spec.ui?.responses ?? {};
  const hasUI = Boolean(spec.ui?.capture ?? Object.keys(uiResponses).length > 0);
  const recordUi = (op: string, payload?: unknown, result?: unknown) => {
    capture.ui.push({ op, payload: toJsonValue(payload), result: toJsonValue(result) });
  };
  const theme = {
    fg: (_token: string, text: string) => String(text ?? ""),
    strikethrough: (text: string) => String(text ?? ""),
  };

  return {
    hasUI,
    cwd,
    ui: {
      notify: (message: unknown, level?: unknown) => {
        recordUi("notify", { message, level });
      },
      setWidget: (id: unknown, widget: unknown) => {
        recordUi("setWidget", { id, widget });
      },
      setStatus: (status: unknown) => {
        recordUi("setStatus", { status });
      },
      custom: async (name: string, payload?: unknown) => {
        const result = choose((uiResponses as any)[name], null);
        recordUi("custom", { name, payload }, result);
        return result;
      },
      select: async (title: string, options: string[]) => {
        const selected = choose((uiResponses as any).select, options?.[0]);
        const result = typeof selected === "string" ? selected : (options?.[0] ?? "");
        recordUi("select", { title, options }, result);
        return result;
      },
      confirm: async (title: string) => {
        const result = Boolean(
          choose((uiResponses as any).confirm, spec.ui?.confirm_default ?? true),
        );
        recordUi("confirm", { title }, result);
        return result;
      },
      dialog: async (title: string, defaultValue = "") => {
        const value = choose((uiResponses as any).dialog, spec.ui?.dialog_default ?? defaultValue);
        const result = typeof value === "string" ? value : defaultValue;
        recordUi("dialog", { title, defaultValue }, result);
        return result;
      },
      theme,
    },
    modelRegistry: {
      getApiKeyForProvider: async (provider: string) => {
        const apiKeys = (spec.model as any)?.api_keys ?? {};
        const value = apiKeys[provider];
        return typeof value === "string" ? value : undefined;
      },
    },
    sessionManager: {
      getState: () => spec.session?.state ?? {},
      getEntries: () => spec.session?.entries ?? [],
      getBranch: () => spec.session?.branch ?? [],
      getLeafEntry: () => {
        const branch = spec.session?.branch ?? [];
        if (branch.length > 0) return branch[branch.length - 1];
        const entries = spec.session?.entries ?? [];
        return entries.length > 0 ? entries[entries.length - 1] : null;
      },
    },
    getSystemPrompt: () => {
      const state = spec.session?.state;
      if (state && typeof state === "object" && !Array.isArray(state)) {
        const prompt = (state as Record<string, JsonValue>).systemPrompt;
        return typeof prompt === "string" ? prompt : "";
      }
      return "";
    },
  };
}

async function fireEventSequence(
  ext: {
    handlers: Map<string, Array<(payload: JsonValue, ctx: any) => Promise<unknown> | unknown>>;
  },
  spec: MockSpec,
  capture: CaptureLog,
  cwd: string,
): Promise<{ ok: boolean; error: string | null }> {
  const sequence = spec.events?.fire_sequence ?? [];
  const errors: string[] = [];

  for (const [index, item] of sequence.entries()) {
    const eventName = typeof item?.event === "string" ? item.event : "";
    const payload = toJsonValue(item?.payload ?? {});
    const eventCapture: EventCapture = {
      event: eventName,
      payload,
      handler_count: 0,
      responses: [],
      response: null,
    };

    if (!eventName) {
      eventCapture.error = `events.fire_sequence[${index}] missing string event`;
      capture.events.push(eventCapture);
      errors.push(eventCapture.error);
      continue;
    }

    const handlers = ext.handlers.get(eventName);
    eventCapture.handler_count = handlers?.length ?? 0;
    if (!handlers || handlers.length === 0) {
      eventCapture.error = `no handlers for event '${eventName}'`;
      capture.events.push(eventCapture);
      errors.push(`events.fire_sequence[${index}] ${eventCapture.error}`);
      continue;
    }

    try {
      const ctx = buildEventContext(spec, capture, cwd);
      for (const handler of handlers) {
        if (typeof handler !== "function") {
          throw new Error(`event '${eventName}' registered a non-function handler`);
        }
        const response = await handler(payload, ctx);
        eventCapture.responses.push(toJsonValue(response));
      }
      eventCapture.response =
        eventCapture.responses.length === 1 ? eventCapture.responses[0] : eventCapture.responses;
      if (Object.hasOwn(item, "expected_response")) {
        eventCapture.expected_response = toJsonValue(item.expected_response);
        eventCapture.expected_match = jsonEquals(eventCapture.response, item.expected_response);
        if (!eventCapture.expected_match) {
          eventCapture.error = `expected_response mismatch for event '${eventName}'`;
          errors.push(`events.fire_sequence[${index}] ${eventCapture.error}`);
        }
      }
    } catch (err) {
      eventCapture.error = err instanceof Error ? err.message : String(err);
      errors.push(
        `events.fire_sequence[${index}] handler error for '${eventName}': ${eventCapture.error}`,
      );
    }

    capture.events.push(eventCapture);
  }

  return errors.length > 0 ? { ok: false, error: errors.join("; ") } : { ok: true, error: null };
}

async function main() {
  applyDeterministicGlobals();
  const args = process.argv.slice(2);
  if (args.length < 2) {
    console.error(
      "Usage: bun run tests/ext_conformance/ts_harness/run_extension.ts <extension-path> <mock-spec-path> [cwd]",
    );
    process.exit(1);
  }

  const extensionPath = path.resolve(args[0]);
  const mockSpecPath = path.resolve(args[1]);
  const envCwd = process.env.PI_DETERMINISTIC_CWD;
  const cwd = args[2] ? path.resolve(args[2]) : envCwd ? path.resolve(envCwd) : process.cwd();

  const spec = normalizeMockSpec(readJson(mockSpecPath));
  const capture: CaptureLog = {
    sendMessage: [],
    sendUserMessage: [],
    appendEntry: [],
    setSessionName: [],
    setLabel: [],
    setActiveTools: [],
    setModel: [],
    setThinkingLevel: [],
    exec: [],
    http: [],
    ui: [],
    events: [],
    warnings: [],
  };

  const restoreFetch = installFetchMock(spec, capture);

  let exitCode = 0;
  try {
    const loadStart = Date.now();
    const loadExtensions = await resolveLoadExtensions();
    let result = await loadExtensions([extensionPath], cwd);
    if (shouldRetryWithFallbackLoader(result)) {
      result = await fallbackLoadExtensions([extensionPath], cwd);
    }
    const loadTimeMs = Date.now() - loadStart;

    if (result.errors.length > 0) {
      originalConsole.log(
        JSON.stringify(
          {
            success: false,
            error: result.errors
              .map((e: { path: string; error: string }) => `${e.path}: ${e.error}`)
              .join("; "),
            extension: null,
            load_time_ms: loadTimeMs,
            logs: CAPTURE_LOGS ? capturedLogs : undefined,
          },
          null,
          2,
        ),
      );
      return;
    }

    if (result.extensions.length === 0) {
      originalConsole.log(
        JSON.stringify(
          {
            success: false,
            error: "No extension loaded (empty result)",
            extension: null,
            load_time_ms: loadTimeMs,
            logs: CAPTURE_LOGS ? capturedLogs : undefined,
          },
          null,
          2,
        ),
      );
      return;
    }

    const ext = result.extensions[0];
    const runtime = result.runtime as {
      sendMessage: (message: JsonValue, options?: JsonValue) => void;
      sendUserMessage: (content: JsonValue, options?: JsonValue) => void;
      appendEntry: (customType: string, data?: JsonValue) => void;
      setSessionName: (name: string) => void;
      getSessionName: () => string | undefined;
      setLabel: (entryId: string, label?: string) => void;
      getActiveTools: () => string[];
      getAllTools: () => Array<{ name: string; description?: string }>;
      setActiveTools: (toolNames: string[]) => void;
      setModel: (model: JsonValue) => Promise<boolean>;
      getThinkingLevel: () => string;
      setThinkingLevel: (level: string) => void;
      exec?: (
        command: string,
        args: string[],
        cwd: string,
        options?: JsonValue,
      ) => Promise<ExecResult>;
      flagValues: Map<string, boolean | string>;
      pendingProviderRegistrations: Array<{
        name: string;
        config: { models?: Array<{ id?: string; name?: string }> };
      }>;
    };

    let sessionName = spec.session?.name ?? (spec.session?.state as any)?.sessionName;
    const acceptSessionMutations = spec.session?.accept_mutations ?? true;
    const acceptModelMutations = spec.model?.accept_mutations ?? true;

    runtime.sendMessage = (message, options) => {
      capture.sendMessage.push({ message, options });
    };
    runtime.sendUserMessage = (content, options) => {
      capture.sendUserMessage.push({ content, options });
    };
    runtime.appendEntry = (customType, data) => {
      capture.appendEntry.push({ customType, data });
      if (acceptSessionMutations && spec.session?.entries) {
        spec.session.entries.push({ customType, data });
      }
    };
    runtime.setSessionName = (name) => {
      capture.setSessionName.push({ name });
      if (acceptSessionMutations) {
        sessionName = name;
        if (spec.session?.state && typeof spec.session.state === "object" && spec.session.state) {
          (spec.session.state as Record<string, JsonValue>)["sessionName"] = name;
        }
      }
    };
    runtime.getSessionName = () => sessionName;
    runtime.setLabel = (entryId, label) => {
      capture.setLabel.push({ entryId, label });
    };
    runtime.getActiveTools = () => spec.tools?.active_tools ?? [];
    runtime.getAllTools = () => spec.tools?.all_tools ?? [];
    runtime.setActiveTools = (toolNames) => {
      capture.setActiveTools.push({ tools: toolNames });
      if (acceptSessionMutations && spec.tools) {
        spec.tools.active_tools = [...toolNames];
      }
    };
    runtime.setModel = async (model) => {
      capture.setModel.push({ model });
      if (acceptModelMutations && spec.model) {
        spec.model.current = {
          ...(spec.model.current ?? {}),
          provider: (model as any)?.provider ?? spec.model.current?.provider,
          model_id: (model as any)?.id ?? (model as any)?.model_id ?? spec.model.current?.model_id,
          name: (model as any)?.name ?? spec.model.current?.name,
        };
      }
      return true;
    };
    runtime.getThinkingLevel = () => spec.model?.thinking_level ?? "off";
    runtime.setThinkingLevel = (level) => {
      capture.setThinkingLevel.push({ level });
      if (acceptModelMutations && spec.model) {
        spec.model.thinking_level = level;
      }
    };
    runtime.exec = async (command, args, execCwd) => {
      const match = pickExecRule(spec.exec?.rules, command, args);
      const resultValue = match?.result ??
        spec.exec?.default_result ?? {
          stdout: "",
          stderr: "mock: command not found",
          code: 127,
          killed: false,
        };
      capture.exec.push({
        command,
        args,
        cwd: execCwd,
        matched: Boolean(match),
        stdout: resultValue.stdout,
        stderr: resultValue.stderr,
        code: resultValue.code,
        killed: resultValue.killed ?? false,
      });
      return resultValue;
    };

    const eventOutcome = await fireEventSequence(ext, spec, capture, cwd);
    if (!eventOutcome.ok) {
      exitCode = 1;
    }

    const handlers: Record<string, number> = {};
    for (const [event, fns] of ext.handlers) {
      handlers[event] = fns.length;
    }

    const tools = [];
    for (const [, registered] of ext.tools) {
      const def = registered.definition as any;
      tools.push({
        name: def.name,
        label: def.label ?? null,
        description: def.description ?? null,
        parameters: def.parameters ?? null,
        hasExecute: typeof def.execute === "function",
      });
    }

    const commands = [];
    for (const [, cmd] of ext.commands) {
      commands.push({
        name: cmd.name,
        description: cmd.description ?? null,
        userFacing: (cmd as any).userFacing ?? false,
        hasHandler: typeof cmd.handler === "function",
      });
    }

    const shortcuts = [];
    for (const [, sc] of ext.shortcuts) {
      shortcuts.push({
        shortcut: sc.shortcut,
        description: sc.description ?? null,
        hasHandler: typeof sc.handler === "function",
      });
    }

    const flags = [];
    for (const [, flag] of ext.flags) {
      flags.push({
        name: flag.name,
        type: flag.type,
        default: (flag as any).default ?? null,
        description: flag.description ?? null,
      });
    }

    const messageRenderers = Array.from(ext.messageRenderers.keys());
    const providers = runtime.pendingProviderRegistrations.map((p) => ({
      name: p.name,
      models: (p.config.models ?? []).map((m) => ({ id: m.id ?? null, name: m.name ?? null })),
    }));
    const flagValues: Record<string, boolean | string> = {};
    for (const [k, v] of runtime.flagValues) {
      flagValues[k] = v;
    }

    const output = {
      success: eventOutcome.ok,
      error: eventOutcome.error,
      load_time_ms: loadTimeMs,
      spec: {
        path: mockSpecPath,
        schema: spec.schema ?? null,
        extension_id: spec.extension_id ?? null,
      },
      extension: {
        path: ext.path,
        resolvedPath: ext.resolvedPath,
        handlers,
        tools,
        commands,
        shortcuts,
        flags,
        messageRenderers,
        providers,
        flagValues,
      },
      runtime: {
        sessionName,
        activeTools: spec.tools?.active_tools ?? [],
        allTools: spec.tools?.all_tools ?? [],
        model: spec.model?.current ?? null,
        thinkingLevel: spec.model?.thinking_level ?? "off",
      },
      capture,
      logs: CAPTURE_LOGS ? capturedLogs : undefined,
    };

    originalConsole.log(JSON.stringify(output, null, 2));
  } catch (err) {
    const output = {
      success: false,
      error: err instanceof Error ? `${err.message}\n${err.stack}` : String(err),
      extension: null,
      load_time_ms: null,
      logs: CAPTURE_LOGS ? capturedLogs : undefined,
    };
    originalConsole.log(JSON.stringify(output, null, 2));
  } finally {
    restoreFetch();
    if (FORCE_EXIT) {
      process.exit(exitCode);
    }
  }
}

main();
