// SCN-node-builtin-apis: Core builtin API availability and behavior

import { createHash, randomBytes } from "node:crypto";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmdirSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, extname, join, resolve } from "node:path";

const result = {
  fixture_id: "builtin-apis-core",
  scenario_id: "SCN-node-builtin-apis",
  checks: [],
};

// fs surface
try {
  const dir = mkdtempSync(join(tmpdir(), "fncompat-"));
  const file = join(dir, "test.txt");
  writeFileSync(file, "hello frankennode");
  const content = readFileSync(file, "utf-8");
  result.checks.push({
    name: "fs_write_read_roundtrip",
    pass: content === "hello frankennode",
    detail: `read back ${content.length} chars`,
  });
  result.checks.push({
    name: "fs_exists_sync",
    pass: existsSync(file),
    detail: "existsSync returns true for written file",
  });
  unlinkSync(file);
  rmdirSync(dir);
  result.checks.push({
    name: "fs_unlink_rmdir",
    pass: !existsSync(file),
    detail: "cleanup succeeded",
  });
} catch (err) {
  result.checks.push({ name: "fs_write_read_roundtrip", pass: false, detail: err.message });
}

// path surface
result.checks.push({
  name: "path_join",
  pass: join("a", "b", "c") === "a/b/c" || join("a", "b", "c") === "a\\b\\c",
  detail: `join result: ${join("a", "b", "c")}`,
});
result.checks.push({
  name: "path_basename",
  pass: basename("/foo/bar/baz.txt") === "baz.txt",
  detail: `basename: ${basename("/foo/bar/baz.txt")}`,
});
result.checks.push({
  name: "path_extname",
  pass: extname("file.tar.gz") === ".gz",
  detail: `extname: ${extname("file.tar.gz")}`,
});

// crypto surface
try {
  const hash = createHash("sha256").update("test").digest("hex");
  result.checks.push({
    name: "crypto_sha256",
    pass: hash === "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
    detail: `sha256 of 'test': ${hash.substring(0, 16)}...`,
  });
  const bytes = randomBytes(16);
  result.checks.push({
    name: "crypto_random_bytes",
    pass: bytes.length === 16,
    detail: `randomBytes(16) produced ${bytes.length} bytes`,
  });
} catch (err) {
  result.checks.push({ name: "crypto_sha256", pass: false, detail: err.message });
}

// process surface
result.checks.push({
  name: "process_pid",
  pass: typeof process.pid === "number" && process.pid > 0,
  detail: `pid: ${process.pid}`,
});
result.checks.push({
  name: "process_env",
  pass: typeof process.env === "object",
  detail: `env keys: ${Object.keys(process.env).length}`,
});
result.checks.push({
  name: "process_argv",
  pass: Array.isArray(process.argv) && process.argv.length >= 1,
  detail: `argv length: ${process.argv.length}`,
});
result.checks.push({
  name: "process_cwd",
  pass: typeof process.cwd() === "string" && process.cwd().length > 0,
  detail: `cwd: ${process.cwd().substring(0, 30)}`,
});

// timers surface
result.checks.push({
  name: "timers_settimeout_exists",
  pass: typeof setTimeout === "function",
  detail: `setTimeout is ${typeof setTimeout}`,
});
result.checks.push({
  name: "timers_setinterval_exists",
  pass: typeof setInterval === "function",
  detail: `setInterval is ${typeof setInterval}`,
});
result.checks.push({
  name: "timers_queuemicrotask_exists",
  pass: typeof queueMicrotask === "function",
  detail: `queueMicrotask is ${typeof queueMicrotask}`,
});

console.log(JSON.stringify(result));
