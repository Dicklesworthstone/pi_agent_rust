// SCN-error-and-diagnostics-parity: Error codes, stack shape, exit behavior
const result = {
  fixture_id: "error-diagnostics-basic",
  scenario_id: "SCN-error-and-diagnostics-parity",
  checks: [],
};

// error-codes surface: ENOENT
import { readFileSync } from "node:fs";

try {
  readFileSync("/nonexistent/path/that/does/not/exist/ever");
  result.checks.push({ name: "enoent_thrown", pass: false, detail: "no error thrown" });
} catch (err) {
  result.checks.push({
    name: "enoent_code",
    pass: err.code === "ENOENT",
    detail: `error.code: ${err.code}`,
  });
  result.checks.push({
    name: "enoent_syscall",
    pass: typeof err.syscall === "string",
    detail: `error.syscall: ${err.syscall}`,
  });
}

// stack-shape surface
try {
  function innerFn() {
    throw new Error("stack test");
  }
  function outerFn() {
    innerFn();
  }
  outerFn();
} catch (err) {
  const lines = err.stack.split("\n");
  result.checks.push({
    name: "stack_first_line_message",
    pass: lines[0].includes("stack test"),
    detail: `first line: ${lines[0]}`,
  });
  result.checks.push({
    name: "stack_has_function_names",
    pass: err.stack.includes("innerFn") && err.stack.includes("outerFn"),
    detail: `contains innerFn: ${err.stack.includes("innerFn")}, outerFn: ${err.stack.includes("outerFn")}`,
  });
  result.checks.push({
    name: "stack_has_at_prefix",
    pass: lines.some((l) => l.trim().startsWith("at ")),
    detail: `has 'at ' prefix in stack frames`,
  });
}

// exit-behavior surface
result.checks.push({
  name: "process_exit_code_default",
  pass: process.exitCode === undefined || process.exitCode === 0,
  detail: `process.exitCode: ${process.exitCode}`,
});
result.checks.push({
  name: "process_exit_function",
  pass: typeof process.exit === "function",
  detail: `process.exit is ${typeof process.exit}`,
});

// TypeError shape
try {
  null.foo;
} catch (err) {
  result.checks.push({
    name: "typeerror_name",
    pass: err.name === "TypeError",
    detail: `error.name: ${err.name}`,
  });
  result.checks.push({
    name: "typeerror_message_shape",
    pass: err.message.includes("null") || err.message.includes("Cannot"),
    detail: `message: ${err.message}`,
  });
}

console.log(JSON.stringify(result));
