import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import {
	EXTENSION_ID,
	REDACTED_VALUE,
	type ArtifactContext,
	type ArtifactPaths,
	type DebugArtifactEnvelope,
	type DebugArtifactKind,
	type ExtensionConfig,
	type RedactOptions,
} from "./types";

const SENSITIVE_KEY_RE = /(authorization|api[-_]?key|token|secret|password|cookie|set-cookie|signature|credential|oauth|auth)/i;
const BEARER_RE = /\bBearer\s+[A-Za-z0-9._\-+/=]+/gi;
const OPENAI_KEY_RE = /\bsk-[A-Za-z0-9\-_]+\b/g;
const HEADER_TOKEN_RE = /\b(x-api-key|api-key|authorization)\b\s*[:=]\s*[^\s,;]+/gi;

function ensureDir(dirPath: string) {
	fs.mkdirSync(dirPath, { recursive: true });
}

function toSessionInfo(context: ArtifactContext) {
	const maybeExtensionContext = context as Pick<ExtensionContext, "cwd" | "sessionManager">;
	const sessionManager = maybeExtensionContext.sessionManager;
	if (sessionManager) {
		return {
			cwd: context.cwd,
			sessionId: sessionManager.getSessionId(),
			sessionFile: sessionManager.getSessionFile(),
			sessionDir: sessionManager.getSessionDir(),
		};
	}
	return context;
}

function sanitizePathSegment(value: string | undefined, fallback: string): string {
	if (!value) return fallback;
	const normalized = value.replace(/[^a-zA-Z0-9._-]+/g, "-").replace(/^-+|-+$/g, "");
	return normalized.length > 0 ? normalized : fallback;
}

function redactInlineSecrets(value: string, placeholder: string): string {
	return value
		.replace(BEARER_RE, `Bearer ${placeholder}`)
		.replace(OPENAI_KEY_RE, placeholder)
		.replace(HEADER_TOKEN_RE, (_match, key: string) => `${key}: ${placeholder}`);
}

function shouldRedactKey(key: string): boolean {
	return SENSITIVE_KEY_RE.test(key);
}

export function redactValue(value: unknown, options: RedactOptions = {}): unknown {
	const placeholder = options.placeholder ?? REDACTED_VALUE;
	const seen = new WeakSet<object>();

	const visit = (input: unknown): unknown => {
		if (typeof input === "string") {
			return redactInlineSecrets(input, placeholder);
		}
		if (!input || typeof input !== "object") {
			return input;
		}
		if (seen.has(input)) {
			return "[Circular]";
		}
		seen.add(input);

		if (Array.isArray(input)) {
			return input.map((item) => visit(item));
		}

		const result: Record<string, unknown> = {};
		for (const [key, item] of Object.entries(input)) {
			result[key] = shouldRedactKey(key) ? placeholder : visit(item);
		}
		return result;
	};

	return visit(value);
}

export function resolveArtifactPaths(settings: ExtensionConfig, context: ArtifactContext): ArtifactPaths {
	const sessionInfo = toSessionInfo(context);
	const rootDir = settings.artifactRoot.startsWith("~/")
		? path.join(os.homedir(), settings.artifactRoot.slice(2))
		: path.resolve(settings.artifactRoot);
	const sessionDir = path.join(rootDir, "sessions", sanitizePathSegment(sessionInfo.sessionId, "no-session"));

	return {
		rootDir,
		sessionDir,
		providerRequestsDir: path.join(sessionDir, "provider-requests"),
		compactResponsesDir: path.join(sessionDir, "compact-responses"),
		compactionDir: path.join(sessionDir, "compaction-events"),
		lifecycleDir: path.join(sessionDir, "lifecycle"),
	};
}

function selectArtifactDirectory(paths: ArtifactPaths, kind: DebugArtifactKind): string {
	switch (kind) {
		case "provider-request":
			return paths.providerRequestsDir;
		case "compact-response":
			return paths.compactResponsesDir;
		case "compaction-event":
			return paths.compactionDir;
		case "lifecycle":
		default:
			return paths.lifecycleDir;
	}
}

function shouldWriteArtifact(kind: DebugArtifactKind, settings: ExtensionConfig): boolean {
	switch (kind) {
		case "provider-request":
			return settings.logProviderPayloads;
		case "compact-response":
			return settings.logCompactResponses;
		case "compaction-event":
		case "lifecycle":
			return settings.debug;
		default:
			return false;
	}
}

export function writeDebugArtifact(
	kind: DebugArtifactKind,
	data: unknown,
	settings: ExtensionConfig,
	context: ArtifactContext,
): string | undefined {
	if (!shouldWriteArtifact(kind, settings)) {
		return undefined;
	}

	const sessionInfo = toSessionInfo(context);
	const paths = resolveArtifactPaths(settings, context);
	const targetDir = selectArtifactDirectory(paths, kind);
	ensureDir(targetDir);

	const timestamp = new Date().toISOString();
	const fileName = `${timestamp.replace(/[.:]/g, "-")}-${kind}.json`;
	const filePath = path.join(targetDir, fileName);
	const envelope: DebugArtifactEnvelope = {
		extension: EXTENSION_ID,
		kind,
		timestamp,
		cwd: sessionInfo.cwd,
		sessionId: sessionInfo.sessionId,
		sessionFile: sessionInfo.sessionFile,
		sessionDir: sessionInfo.sessionDir,
		redaction: {
			enabled: settings.redactSensitiveData,
		},
		data: settings.redactSensitiveData ? redactValue(data) : data,
	};

	fs.writeFileSync(filePath, `${JSON.stringify(envelope, null, 2)}\n`, "utf8");
	return filePath;
}
