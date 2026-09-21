// SCN-module-resolution-esm-cjs: ESM import behavior

import { existsSync } from "node:fs";
import { join } from "node:path";

const result = {
  fixture_id: "esm-import-basic",
  scenario_id: "SCN-module-resolution-esm-cjs",
  surface: "esm",
  checks: [],
};

// Check 1: named import from node:path works
result.checks.push({
  name: "named_import_node_path",
  pass: typeof join === "function",
  detail: `join is ${typeof join}`,
});

// Check 2: named import from node:fs works
result.checks.push({
  name: "named_import_node_fs",
  pass: typeof existsSync === "function",
  detail: `existsSync is ${typeof existsSync}`,
});

// Check 3: dynamic import works
try {
  const os = await import("node:os");
  result.checks.push({
    name: "dynamic_import_node_os",
    pass: typeof os.hostname === "function",
    detail: `hostname is ${typeof os.hostname}`,
  });
} catch (err) {
  result.checks.push({
    name: "dynamic_import_node_os",
    pass: false,
    detail: err.message,
  });
}

// Check 4: import.meta.url is defined
result.checks.push({
  name: "import_meta_url",
  pass: typeof import.meta.url === "string" && import.meta.url.length > 0,
  detail: `import.meta.url starts with ${(import.meta.url || "").substring(0, 7)}`,
});

console.log(JSON.stringify(result));
