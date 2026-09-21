// SCN-module-resolution-esm-cjs: CJS require behavior
const result = {
  fixture_id: "cjs-require-basic",
  scenario_id: "SCN-module-resolution-esm-cjs",
  surface: "cjs",
  checks: [],
};

// Check 1: require node:path
try {
  const path = require("node:path");
  result.checks.push({
    name: "require_node_path",
    pass: typeof path.join === "function",
    detail: `path.join is ${typeof path.join}`,
  });
} catch (err) {
  result.checks.push({ name: "require_node_path", pass: false, detail: err.message });
}

// Check 2: require without node: prefix
try {
  const fs = require("fs");
  result.checks.push({
    name: "require_fs_no_prefix",
    pass: typeof fs.readFileSync === "function",
    detail: `fs.readFileSync is ${typeof fs.readFileSync}`,
  });
} catch (err) {
  result.checks.push({ name: "require_fs_no_prefix", pass: false, detail: err.message });
}

// Check 3: __filename and __dirname are defined
result.checks.push({
  name: "cjs_globals_filename",
  pass: typeof __filename === "string" && __filename.length > 0,
  detail: `__filename defined: ${typeof __filename === "string"}`,
});

result.checks.push({
  name: "cjs_globals_dirname",
  pass: typeof __dirname === "string" && __dirname.length > 0,
  detail: `__dirname defined: ${typeof __dirname === "string"}`,
});

// Check 4: module.exports works
const exported = { value: 42 };
module.exports = exported;
result.checks.push({
  name: "module_exports_assignment",
  pass: module.exports === exported,
  detail: `module.exports identity preserved: ${module.exports === exported}`,
});

console.log(JSON.stringify(result));
