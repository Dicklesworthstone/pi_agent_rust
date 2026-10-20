// Private callback registries; the native side holds no JavaScript values.
((schedule, cancel, enqueue) => {
    const PromiseType = Promise;
    const ErrorType = Error;
    const TypeErrorType = TypeError;
    const RangeErrorType = RangeError;
    const isArray = Array.isArray;
    const number = Number;
    const finite = Number.isFinite;
    const integer = Number.isInteger;
    const stringify = JSON.stringify;
    const parse = JSON.parse;
    const hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    const keys = Object.keys;
    const freeze = Object.freeze;
    const now = Date.now;
    const timers = new Map();
    const calls = new Map();
    const timerSet = timers.set.bind(timers);
    const timerGet = timers.get.bind(timers);
    const timerDelete = timers.delete.bind(timers);
    const timerClear = timers.clear.bind(timers);
    const callSet = calls.set.bind(calls);
    const callGet = calls.get.bind(calls);
    const callDelete = calls.delete.bind(calls);
    const callClear = calls.clear.bind(calls);

    const addTimer = (repeat, callback, delay = 0, ...args) => {
        if (typeof callback !== 'function') {
            throw new TypeErrorType('Timer callbacks must be functions; strings are not evaluated');
        }
        const ms = number(delay);
        if (!finite(ms)) throw new RangeErrorType('EVAL_TIMER_DELAY: delay must be finite');
        const id = schedule(ms < 0 ? 0 : ms, repeat);
        timerSet(id, () => callback(...args));
        return id;
    };
    const clearTimer = id => {
        if (!finite(id) || !integer(id) || id < 1 || id > 4294967295) return;
        cancel(id);
        timerDelete(id);
    };
    const timeout = (callback, delay = 0, ...args) => addTimer(false, callback, delay, ...args);
    globalThis.setTimeout = timeout;
    globalThis.clearTimeout = clearTimer;
    globalThis.setInterval = (callback, delay = 0, ...args) => addTimer(true, callback, delay, ...args);
    globalThis.clearInterval = clearTimer;
    globalThis.sleep = (delay = 0) => new PromiseType(resolve => timeout(resolve, delay));
    const callTool = (name, args, structured) => new PromiseType((resolve, reject) => {
        if (typeof name !== 'string' || args === null || typeof args !== 'object' || isArray(args)) {
            throw new TypeErrorType('tool.call/tool.result requires a tool name and object arguments');
        }
        const id = structured
            ? enqueue('__pi_tool_result', stringify({name, args}))
            : enqueue(name, stringify(args));
        callSet(id, {resolve, reject, structured});
    });
    globalThis.tool.call = (name, args = {}) => callTool(name, args, false);
    globalThis.tool.result = (name, args = {}) => callTool(name, args, true);

    // Retain only opaque job ids. Every operation re-enters the live native
    // host, which checks session ownership, enabled tools and current policy.
    const resultText = result => result.content
        .filter(block => block.type === 'text').map(block => block.text).join('\n');
    const nativeResult = async (name, args) => {
        const result = await callTool(name, args, true);
        if (result.isError) throw new ErrorType(resultText(result) || 'Agent job operation failed');
        return result.details;
    };
    const waitMilliseconds = timeout => {
        if (typeof timeout !== 'number' || !finite(timeout) || timeout < 0 || timeout > 600) {
            throw new RangeErrorType('EVAL_AGENT_INPUT: timeout must be finite seconds between 0 and 600');
        }
        return Math.floor(timeout * 1000);
    };
    const readJob = async (id, action, timeoutMs) => {
        const args = {action, jobId:id};
        if (timeoutMs !== undefined) args.timeoutMs = timeoutMs;
        const snapshot = await nativeResult('jobs', args);
        if (!snapshot || snapshot.schema !== 'pi.host_job.v1' || snapshot.id !== id
            || typeof snapshot.status !== 'string') {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid native job snapshot');
        }
        return snapshot;
    };
    const waitJob = async (job, timeoutMs, raiseErrors) => {
        const snapshot = await readJob(job.id, 'wait', timeoutMs);
        if (snapshot.status === 'running') {
            const error = new ErrorType('EVAL_AGENT_TIMEOUT: job is still running; it remains available through jobs');
            error.name = 'TimeoutError';
            throw error;
        }
        const result = snapshot.result;
        if (!result || !isArray(result.content) || !hasOwn(result, 'details')
            || (hasOwn(result, 'isError') && typeof result.isError !== 'boolean')) {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: settled job has no native result');
        }
        if (raiseErrors && (snapshot.status !== 'exited' || result.isError)) {
            throw new ErrorType(resultText(result) || `Agent job ${snapshot.status}`);
        }
        return result;
    };
    class AgentJob {
        constructor(id, agent) {
            this.id = id;
            this.agent = agent;
            freeze(this);
        }
        async status() { return readJob(this.id, 'wait', 0); }
        async done() { return (await this.status()).status !== 'running'; }
        async output() { return (await this.status()).result ?? null; }
        async cancel() { return readJob(this.id, 'cancel'); }
        async wait(timeout = 30) { return waitJob(this, waitMilliseconds(timeout), true); }
    }
    const agentOptions = new Set([
        'agent', 'cwd', 'isolation', 'isoApply', 'outputSchema', 'schemaMode', 'timeoutSeconds', 'scope',
    ]);
    const hasAgentOption = agentOptions.has.bind(agentOptions);
    globalThis.agent = async (prompt, options = {}) => {
        if (typeof prompt !== 'string' || !prompt.trim() || !options
            || typeof options !== 'object' || isArray(options)) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: agent requires a nonempty prompt and an options object');
        }
        const args = {agent:'task'};
        for (const name of keys(options)) {
            if (!hasAgentOption(name)) throw new TypeErrorType(`EVAL_AGENT_INPUT: unknown option ${name}`);
            args[name] = options[name];
        }
        if (typeof args.agent !== 'string' || !args.agent) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: agent must be a nonempty name');
        }
        // Callers cannot overwrite the operation or turn a launch foreground.
        args.task = prompt;
        args.async = true;
        const started = await nativeResult('subagent', args);
        if (!started || started.schema !== 'pi.subagent.background.v1'
            || typeof started.jobId !== 'string' || !started.jobId) {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid background acknowledgment');
        }
        return new AgentJob(started.jobId, args.agent);
    };
    globalThis.wait = async (handles, options = {}) => {
        if (!isArray(handles) || handles.length > 32 || handles.some(handle => !(handle instanceof AgentJob))
            || !options || typeof options !== 'object' || isArray(options)
            || keys(options).some(name => name !== 'timeout' && name !== 'raiseErrors')) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: wait requires up to 32 agent handles and timeout/raiseErrors options');
        }
        const timeoutMs = waitMilliseconds(options.timeout === undefined ? 30 : options.timeout);
        const raiseErrors = options.raiseErrors === undefined ? true : options.raiseErrors;
        if (typeof raiseErrors !== 'boolean') throw new TypeErrorType('EVAL_AGENT_INPUT: raiseErrors must be boolean');
        const deadline = now() + timeoutMs;
        const results = [];
        for (const handle of handles) {
            results.push(await waitJob(handle, Math.max(0, deadline - now()), raiseErrors));
        }
        return results;
    };
    return {
        dispatch(id, timer, flag, content) {
            if (timer) {
                const callback = timerGet(id);
                if (!flag) timerDelete(id);
                if (callback) callback();
            } else {
                const call = callGet(id);
                callDelete(id);
                if (call) {
                    if (!flag) {
                        call.reject(new ErrorType(content));
                    } else if (!call.structured) {
                        call.resolve(content);
                    } else {
                        try {
                            const result = parse(content);
                            if (result === null || typeof result !== 'object' || isArray(result)
                                || result.schema !== 'pi.eval.tool_result.v1'
                                || !isArray(result.content) || !hasOwn(result, 'details')
                                || typeof result.isError !== 'boolean') {
                                throw new ErrorType('EVAL_BRIDGE_PROTOCOL: invalid structured tool result');
                            }
                            call.resolve(result);
                        } catch (_) {
                            call.reject(new ErrorType('EVAL_BRIDGE_PROTOCOL: invalid structured tool result'));
                        }
                    }
                }
            }
        },
        clear() {
            timerClear();
            callClear();
        },
    };
})(__pi_set_timer, __pi_clear_timer, __pi_async_call);
