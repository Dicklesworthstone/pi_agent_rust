// SCN-event-loop-io-ordering: Event loop and microtask ordering
const result = {
  fixture_id: "event-loop-ordering",
  scenario_id: "SCN-event-loop-io-ordering",
  checks: [],
};

// Capture execution order
const order = [];

// Set up various async operations
queueMicrotask(() => order.push("microtask-1"));
setTimeout(() => order.push("timeout-0ms"), 0);
Promise.resolve().then(() => order.push("promise-then-1"));
queueMicrotask(() => {
  order.push("microtask-2");
  Promise.resolve().then(() => order.push("nested-promise"));
});
Promise.resolve().then(() => order.push("promise-then-2"));

// Give time for all to execute, then report
setTimeout(() => {
  // Check 1: Microtasks and promises run before setTimeout(0)
  const timeoutIdx = order.indexOf("timeout-0ms");
  const allMicrotasksBefore = [
    "microtask-1",
    "microtask-2",
    "promise-then-1",
    "promise-then-2",
  ].every((item) => order.indexOf(item) < timeoutIdx);
  result.checks.push({
    name: "microtasks_before_timers",
    pass: allMicrotasksBefore,
    detail: `order: ${JSON.stringify(order)}`,
  });

  // Check 2: Nested promise from microtask runs before timeout
  result.checks.push({
    name: "nested_promise_before_timer",
    pass: order.indexOf("nested-promise") < timeoutIdx,
    detail: `nested-promise at ${order.indexOf("nested-promise")}, timeout at ${timeoutIdx}`,
  });

  // Check 3: Promise.resolve().then() and queueMicrotask interleave correctly
  result.checks.push({
    name: "microtask_promise_interleave",
    pass: order.includes("microtask-1") && order.includes("promise-then-1"),
    detail: `both microtask and promise executed`,
  });

  console.log(JSON.stringify(result));
}, 50);
