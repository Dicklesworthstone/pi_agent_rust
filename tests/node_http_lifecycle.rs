//! Terminal ownership for node:http relay requests (issue #270).
//! The shared cases replace only fetch's I/O boundary. The final test also
//! checks cancellation of a real PiJS streaming hostcall after headers.

use pi::extensions_js::PiJsRuntime;
use pi::scheduler::{DeterministicClock, HostcallOutcome};
use serde_json::{Value, json};

// Also executed against the exact embedded module by the Node diagnostic.
const LIFECYCLE_CASES_JS: &str = r#"
globalThis.__httpLifecycleCases = async function(http) {
  const savedFetch = globalThis.__pi_fetch_for_node_http;
  const results = [];
  const flush = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); };
  const check = (condition, message) => { if (!condition) throw new Error(message); };
  const run = async (name, action) => {
    try { await action(); results.push({ name, passed: true }); }
    catch (error) { results.push({ name, passed: false, error: String(error) }); }
  };
  function bodyFixture(chunks = [], pending = false, readError = null) {
    const state = { reads: 0, cancels: 0, releases: 0, locked: false, canceled: false };
    let position = 0;
    let waiting = null;
    const cancel = () => {
      state.cancels++;
      state.canceled = true;
      if (waiting) { const resolve = waiting; waiting = null; resolve({ done: true }); }
      return Promise.resolve();
    };
    const reader = {
      read() {
        state.reads++;
        if (state.canceled) return Promise.resolve({ done: true });
        if (position < chunks.length) return Promise.resolve({ done: false, value: chunks[position++] });
        if (readError) return Promise.reject(readError);
        if (pending) return new Promise(resolve => { waiting = resolve; });
        return Promise.resolve({ done: true });
      },
      cancel,
      releaseLock() { state.releases++; state.locked = false; },
    };
    return {
      state,
      getReader() {
        if (state.locked) throw new Error('body already locked');
        state.locked = true;
        return reader;
      },
      cancel() {
        if (state.locked) return Promise.reject(new Error('cancel requires reader ownership'));
        return cancel();
      },
    };
  }
  const response = body => ({ status: 200, headers: { entries: () => [] }, body });
  function observe(req) {
    const events = { errors: [], closed: 0 };
    req.on('error', error => events.errors.push(error));
    req.on('close', () => events.closed++);
    return events;
  }
  try {
    await run('HTTP events preserve once ordering and listener receiver', async () => {
      const req = http.request('https://relay.invalid/');
      const calls = [];
      req.on('probe', function(value) {
        check(this === req, 'listener receives emitter'); calls.push('normal:' + value);
      });
      req.prependOnceListener('probe', function(value) {
        check(this === req, 'once listener receives emitter'); calls.push('once:' + value);
      });
      check(req.emit('probe', 1) && req.emit('probe', 2), 'emit reports listeners');
      check(!req.emit('missing'), 'empty emit returns false');
      check(calls.join(',') === 'once:1,normal:1,normal:2', 'once and prepend preserved');
      check(req.listenerCount('probe') === 1, 'once wrapper removed');
      req.destroy();
    });
    await run('HTTP listener changes apply to the next dispatch', async () => {
      const req = http.request('https://relay.invalid/');
      const calls = [];
      const second = () => calls.push('second');
      const later = () => calls.push('later');
      req.once('probe', () => {
        calls.push('first'); req.removeListener('probe', second); req.on('probe', later);
      });
      req.on('probe', second);
      req.emit('probe'); req.emit('probe');
      check(calls.join(',') === 'first,second,later', 'dispatch uses a listener snapshot');
      req.destroy();
    });
    await run('HTTP listener exceptions stop dispatch and keep once removal', async () => {
      const req = http.request('https://relay.invalid/');
      const failure = new Error('observer failure');
      let after = 0, errorEvents = 0;
      req.once('probe', () => { throw failure; });
      req.on('probe', () => after++);
      req.on('error', () => errorEvents++);
      let caught;
      try { req.emit('probe'); } catch (error) { caught = error; }
      check(caught === failure && after === 0 && errorEvents === 0, 'throw reaches teardown boundary');
      req.emit('probe');
      check(after === 1 && req.listenerCount('probe') === 1, 'throwing once listener removed');
      req.destroy();
    });
    await run('fetch rejection is terminal', async () => {
      let signal;
      globalThis.__pi_fetch_for_node_http = (_url, options) => {
        signal = options.signal;
        return Promise.reject(new Error('relay refused'));
      };
      const req = http.request('https://relay.invalid/');
      const events = observe(req);
      req.end(); await flush();
      check(events.closed === 1 && events.errors.length === 1, 'one error and one close');
      check(events.errors[0].message === 'relay refused', 'original error');
      check(req.destroyed && signal.aborted && req._timeoutId === null, 'transport released');
      req.destroy();
      check(events.closed === 1, 'later destruction is idempotent');
    });
    await run('response callback throw cancels unread body', async () => {
      const body = bodyFixture([], true);
      let signal;
      globalThis.__pi_fetch_for_node_http = (_url, options) => {
        signal = options.signal; return Promise.resolve(response(body));
      };
      let incoming;
      const req = http.request('https://relay.invalid/', res => {
        incoming = res; throw new Error('response observer failed');
      });
      const events = observe(req);
      req.end(); await flush();
      check(body.state.cancels === 1 && body.state.reads === 0, 'unread body canceled exactly once');
      check(events.closed === 1 && events.errors.length === 1, 'request finalized');
      check(incoming.destroyed && !incoming.complete && signal.aborted, 'not a successful response');
    });
    await run('header iteration failure releases body', async () => {
      const body = bodyFixture([], true);
      globalThis.__pi_fetch_for_node_http = () => Promise.resolve({
        status: 200, body, headers: { entries() { throw new Error('bad response headers'); } },
      });
      const req = http.request('https://relay.invalid/');
      const events = observe(req);
      req.end(); await flush();
      check(body.state.cancels === 1 && events.closed === 1, 'body owned before header iteration');
      check(events.errors.length === 1, 'failure reported');
    });
    await run('finish listener failure aborts pending headers', async () => {
      let signal;
      globalThis.__pi_fetch_for_node_http = (_url, options) => {
        signal = options.signal; return new Promise(() => {});
      };
      const req = http.request('https://relay.invalid/');
      const events = observe(req);
      req.on('finish', () => { throw new Error('finish observer failed'); });
      req.end(); await flush();
      check(events.closed === 1 && events.errors.length === 1 && signal.aborted, 'pending transport cancelled');
    });
    await run('end listener failure still closes response and request', async () => {
      const body = bodyFixture();
      globalThis.__pi_fetch_for_node_http = () => Promise.resolve(response(body));
      let closed = 0;
      const req = http.request('https://relay.invalid/', res => {
        res.on('end', () => { throw new Error('end observer failed'); });
        res.on('close', () => closed++);
      });
      const events = observe(req);
      req.end(); await flush();
      check(closed === 1 && events.closed === 1 && events.errors.length === 1, 'end failure finalized');
      check(!body.state.locked, 'reader released despite end failure');
    });
    await run('data observer failure cancels remaining frames', async () => {
      const body = bodyFixture([new Uint8Array([1]), new Uint8Array([2])], true);
      globalThis.__pi_fetch_for_node_http = () => Promise.resolve(response(body));
      let data = 0, ended = 0, closed = 0, responseErrors = 0;
      const req = http.request('https://relay.invalid/', res => {
        res.on('data', () => { data++; throw new Error('data observer failed'); });
        res.on('end', () => ended++);
        res.on('close', () => closed++);
        res.on('error', () => responseErrors++);
      });
      const events = observe(req);
      req.end(); await flush();
      check(data === 1 && ended === 0, 'no later bytes or successful end');
      check(closed === 1 && responseErrors === 1 && events.closed === 1, 'both lifecycles finalized');
      check(events.errors.length === 1 && body.state.cancels === 1, 'request failure and cancellation');
      check(!body.state.locked && body.state.releases === 1, 'reader released');
    });
    await run('body failure reaches request even when response handles error', async () => {
      const failure = new Error('relay disconnected');
      const body = bodyFixture([new Uint8Array([1])], false, failure);
      globalThis.__pi_fetch_for_node_http = () => Promise.resolve(response(body));
      let ended = 0;
      const req = http.request('https://relay.invalid/', res => {
        res.on('error', () => {}); res.on('end', () => ended++);
      });
      const events = observe(req);
      req.end(); await flush();
      check(events.errors.length === 1 && events.errors[0] === failure, 'original failure reaches request');
      check(events.closed === 1 && ended === 0 && !body.state.locked, 'not a successful end');
    });
    for (const empty of [false, true]) {
      await run(`end listener destruction closes once (null body=${empty})`, async () => {
        const body = empty ? null : bodyFixture();
        let signal;
        globalThis.__pi_fetch_for_node_http = (_url, options) => {
          signal = options.signal; return Promise.resolve(response(body));
        };
        let closed = 0, ended = 0;
        const req = http.request('https://relay.invalid/', res => {
          res.on('end', () => { ended++; res.destroy(); });
          res.on('close', () => closed++);
        });
        const events = observe(req);
        req.end(); await flush();
        check(ended === 1 && closed === 1 && events.closed === 1, 'exactly one end/close');
        check(events.errors.length === 0 && !signal.aborted, 'normal EOF is not abort');
        req.destroy();
        check(events.closed === 1, 'later request destruction is idempotent');
      });
    }
    await run('successful binary response completes without abort', async () => {
      const body = bodyFixture([new Uint8Array([0, 255, 65]), new Uint8Array([66])]);
      let signal, incoming;
      globalThis.__pi_fetch_for_node_http = (_url, options) => {
        signal = options.signal; return Promise.resolve(response(body));
      };
      const bytes = [];
      let closed = 0, ended = 0;
      const req = http.request('https://relay.invalid/', res => {
        incoming = res;
        res.on('data', chunk => bytes.push(...chunk));
        res.on('end', () => ended++); res.on('close', () => closed++);
      });
      const events = observe(req);
      req.end(); await flush();
      check(JSON.stringify(bytes) === '[0,255,65,66]', 'binary content unchanged');
      check(ended === 1 && closed === 1 && events.closed === 1 && incoming.complete, 'successful completion');
      check(!signal.aborted && body.state.cancels === 0 && !body.state.locked, 'no successful transport abort');
      check(events.errors.length === 0 && req._timeoutId === null, 'no spurious error or timer');
      req.destroy(); incoming.destroy();
      check(closed === 1 && events.closed === 1 && !signal.aborted, 'completed lifecycles are idempotent');
    });
    await run('error listener throw cannot skip request close', async () => {
      const req = http.request('https://relay.invalid/');
      let closed = 0;
      req.on('close', () => closed++);
      req.on('error', () => { throw new Error('observer failed'); });
      try { req.destroy(new Error('failed')); } catch (_) {}
      req.destroy();
      check(closed === 1 && req.destroyed, 'request close survives observer');
    });
    await run('error listener throw cannot skip response close', async () => {
      const res = new http.IncomingMessage(200, {}, '');
      let closed = 0;
      res.on('close', () => closed++);
      res.on('error', () => { throw new Error('observer failed'); });
      try { res.destroy(new Error('failed')); } catch (_) {}
      res.destroy();
      check(closed === 1 && res.destroyed, 'response close survives observer');
    });
    await run('response close listener throw cannot skip request cleanup', async () => {
      const body = bodyFixture([], true);
      globalThis.__pi_fetch_for_node_http = () => Promise.resolve(response(body));
      const req = http.request('https://relay.invalid/', res => {
        res.on('close', () => { throw new Error('close observer failed'); });
      });
      const events = observe(req);
      req.end(); await flush();
      try { req.destroy(new Error('caller cancelled')); } catch (_) {}
      await flush();
      check(events.closed === 1 && events.errors.length === 1, 'request receives terminal events');
      check(body.state.cancels === 1 && !body.state.locked && req._timeoutId === null, 'transport released');
    });
    await run('late fetch after cancellation disposes body', async () => {
      const body = bodyFixture([], true);
      let resolveFetch;
      globalThis.__pi_fetch_for_node_http = () => new Promise(resolve => { resolveFetch = resolve; });
      let responses = 0;
      const req = http.request('https://relay.invalid/', () => responses++);
      const events = observe(req);
      req.end(); req.abort(); req.abort();
      resolveFetch(response(body)); await flush();
      check(body.state.cancels === 1 && body.state.reads === 0, 'late body disposed');
      check(responses === 0 && events.closed === 1 && events.errors.length === 0, 'no late response');
    });
    for (const paused of [false, true]) {
      await run(`abort releases pending response (paused=${paused})`, async () => {
        const body = bodyFixture([], true);
        globalThis.__pi_fetch_for_node_http = () => Promise.resolve(response(body));
        let closed = 0, ended = 0;
        const req = http.request('https://relay.invalid/', res => {
          if (paused) res.pause();
          res.on('close', () => closed++); res.on('end', () => ended++);
        });
        const events = observe(req);
        req.end(); await flush();
        check(body.state.reads === (paused ? 0 : 1), 'pause does not pull body');
        req.destroy(); req.destroy(); await flush();
        check(closed === 1 && ended === 0 && events.closed === 1, 'abort is terminal without end');
        check(body.state.cancels === 1 && body.state.releases === 1 && !body.state.locked, 'cancel and release exactly once');
      });
    }
    await run('timeout closes a pending body and clears timer', async () => {
      const savedSet = globalThis.setTimeout, savedClear = globalThis.clearTimeout;
      const timers = new Map();
      let nextId = 0;
      globalThis.setTimeout = fn => { timers.set(++nextId, fn); return nextId; };
      globalThis.clearTimeout = id => timers.delete(id);
      try {
        const body = bodyFixture([], true);
        globalThis.__pi_fetch_for_node_http = () => Promise.resolve(response(body));
        const req = http.request('https://relay.invalid/', { timeout: 25 });
        const events = observe(req);
        let timedOut = 0;
        req.on('timeout', () => timedOut++);
        req.end(); await flush();
        check(timers.size === 1, 'one active inactivity timer');
        timers.values().next().value(); await flush();
        check(timedOut === 1 && events.closed === 1 && events.errors[0].code === 'ETIMEDOUT', 'timeout delivered');
        check(body.state.cancels === 1 && !body.state.locked && timers.size === 0, 'timeout releases all ownership');
      } finally {
        globalThis.setTimeout = savedSet; globalThis.clearTimeout = savedClear;
      }
    });
    await run('http.get observes preflight rejection without egress', async () => {
      let fetches = 0;
      globalThis.__pi_fetch_for_node_http = () => { fetches++; throw new Error('unexpected egress'); };
      const req = http.get('https://relay.invalid/', { rejectUnauthorized: false });
      const events = observe(req);
      await flush();
      check(fetches === 0 && events.closed === 1 && events.errors.length === 1, 'rejected before fetch');
      check(events.errors[0].message.includes('host networking'), 'host trust policy preserved');
    });
  } finally {
    globalThis.__pi_fetch_for_node_http = savedFetch;
  }
  return results;
};
"#;

#[test]
fn node_http_terminal_lifecycle_contract() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(LIFECYCLE_CASES_JS)
            .await
            .expect("case definitions");
        runtime
            .eval(
                "globalThis.lifecycleResults = null; \
                 import('node:http').then(__httpLifecycleCases).then(\
                   results => { lifecycleResults = results; }, \
                   error => { lifecycleResults = [{ passed: false, error: String(error) }]; }\
                 );",
            )
            .await
            .expect("start lifecycle cases");
        for _ in 0..32 {
            runtime.drain_microtasks().await.expect("drain cases");
            let result = runtime
                .get_global_json("lifecycleResults")
                .await
                .expect("results");
            if !result.is_null() {
                let cases = result.as_array().expect("case array");
                assert_eq!(cases.len(), 21, "all cases must finish: {result}");
                assert!(
                    cases.iter().all(|case| case["passed"] == true),
                    "lifecycle failure: {result}"
                );
                assert!(runtime.drain_hostcall_requests().is_empty());
                return;
            }
        }
        panic!("HTTP lifecycle cases did not settle");
    });
}

#[test]
fn response_callback_failure_cancels_real_streaming_hostcall() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r#"
                globalThis.lifecycleResult = { errors: [], closed: 0 };
                import('node:http').then(http => {
                    const request = http.get('https://relay.example/stream', () => {
                        throw new Error('consumer rejected response');
                    });
                    request.on('error', error => lifecycleResult.errors.push(error.message));
                    request.on('close', () => { lifecycleResult.closed++; });
                });
                "#,
            )
            .await
            .expect("start request");
        runtime.drain_microtasks().await.expect("enqueue request");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        let request = requests.first().expect("request");
        assert_eq!(request.payload["stream"], true);
        assert!(runtime.is_hostcall_active(&request.call_id));
        runtime.complete_hostcall(
            &request.call_id,
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: json!({ "status": 200, "headers": {} }),
                is_final: false,
            },
        );
        runtime.tick().await.expect("deliver headers");
        runtime
            .drain_microtasks()
            .await
            .expect("cancel failed consumer");
        assert!(!runtime.is_hostcall_active(&request.call_id));
        let result: Value = runtime
            .get_global_json("lifecycleResult")
            .await
            .expect("result");
        assert_eq!(
            result,
            json!({ "errors": ["consumer rejected response"], "closed": 1 })
        );
        assert!(runtime.drain_hostcall_requests().is_empty());
    });
}
