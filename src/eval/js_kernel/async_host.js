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
    const resolvedPromise = Promise.resolve.bind(Promise);
    const promiseThen = Function.prototype.call.bind(Promise.prototype.then);
    let cellGeneration = 0;
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

    // Retain only opaque job/worker ids. Every operation re-enters the live native
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
    const workerStates = new WeakMap();
    const workerGet = workerStates.get.bind(workerStates);
    const workerSet = workerStates.set.bind(workerStates);
    const workerStatuses = new Set(['starting', 'running', 'idle', 'closed', 'failed']);
    const turnStatuses = new Set(['queued', 'running', 'completed', 'failed', 'cancelled']);
    const validWorkerStatus = workerStatuses.has.bind(workerStatuses);
    const validTurnStatus = turnStatuses.has.bind(turnStatuses);
    const validTurnId = id => integer(id) && id > 0 && id <= 32;
    const checkWorker = (snapshot, workerId, turnId) => {
        if (!snapshot || snapshot.schema !== 'pi.subagent.worker.v1'
            || typeof snapshot.workerId !== 'string' || !snapshot.workerId
            || (workerId !== undefined && snapshot.workerId !== workerId)
            || typeof snapshot.jobId !== 'string' || !snapshot.jobId
            || typeof snapshot.agent !== 'string' || !snapshot.agent
            || !validWorkerStatus(snapshot.status) || !validTurnId(snapshot.turnId)
            || (turnId !== undefined && snapshot.turnId !== turnId)
            || !snapshot.turn || snapshot.turn.id !== snapshot.turnId
            || !validTurnStatus(snapshot.turn.status) || !hasOwn(snapshot.turn, 'result')) {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid native worker snapshot');
        }
        const result = snapshot.turn.result;
        if (result === null && snapshot.turn.status !== 'queued' && snapshot.turn.status !== 'running') {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: settled worker turn has no native result');
        }
        if (result !== null && (!result || !isArray(result.content) || !hasOwn(result, 'details')
            || (hasOwn(result, 'isError') && typeof result.isError !== 'boolean'))) {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid native worker result');
        }
        return snapshot;
    };
    const readWorker = async (handle, operation, timeoutMs) => {
        const args = {operation, workerId:handle.workerId};
        if (operation !== 'worker_close') args.turnId = handle.turnId;
        if (timeoutMs !== undefined) args.timeoutMs = timeoutMs;
        const snapshot = checkWorker(await nativeResult('subagent', args), handle.workerId,
            operation === 'worker_close' ? undefined : handle.turnId);
        if (snapshot.jobId !== handle.jobId || snapshot.agent !== handle.agent) {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: native worker identity changed');
        }
        return snapshot;
    };
    const turnDone = turn => turn.status !== 'queued' && turn.status !== 'running';
    const waitTurn = async (turn, timeoutMs, raiseErrors) => {
        const snapshot = await readWorker(turn, 'worker_wait', timeoutMs);
        if (!turnDone(snapshot.turn)) {
            const error = new ErrorType('EVAL_AGENT_TIMEOUT: worker turn is still pending; the worker remains available');
            error.name = 'TimeoutError';
            throw error;
        }
        const result = snapshot.turn.result;
        if (result === null) throw new ErrorType('EVAL_AGENT_PROTOCOL: settled worker turn has no native result');
        if (raiseErrors && (snapshot.turn.status !== 'completed' || result.isError)) {
            throw new ErrorType(resultText(result) || `Agent turn ${snapshot.turn.status}`);
        }
        return result;
    };
    class AgentTurn {
        constructor(worker, turnId) {
            this.id = turnId;
            this.turnId = turnId;
            this.workerId = worker.id;
            this.jobId = worker.jobId;
            this.agent = worker.agent;
            freeze(this);
        }
        async status() { return readWorker(this, 'worker_status'); }
        async done() { return turnDone((await this.status()).turn); }
        async output() { return (await this.status()).turn.result; }
        async wait(timeout = 30) { return waitTurn(this, waitMilliseconds(timeout), true); }
        // Native cancellation closes the worker, including all queued turns.
        async close() { return readWorker(this, 'worker_close'); }
        async cancel() { return this.close(); }
    }
    const currentTurn = worker => new AgentTurn(worker, workerGet(worker));
    class AgentWorker {
        constructor(snapshot) {
            this.id = snapshot.workerId;
            this.workerId = snapshot.workerId;
            this.jobId = snapshot.jobId;
            this.agent = snapshot.agent;
            workerSet(this, snapshot.turnId);
            freeze(this);
        }
        get turnId() { return workerGet(this); }
        async send(prompt) {
            if (typeof prompt !== 'string' || !prompt.trim()) {
                throw new TypeErrorType('EVAL_AGENT_INPUT: send requires a nonempty prompt');
            }
            const previous = workerGet(this);
            const snapshot = checkWorker(await nativeResult('subagent', {
                operation:'worker_send', workerId:this.id, task:prompt,
            }), this.id);
            if (snapshot.jobId !== this.jobId || snapshot.agent !== this.agent || snapshot.turnId <= previous) {
                throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid worker send acknowledgment');
            }
            // Concurrent sends may resolve out of order. The newest native
            // turn remains selected without changing earlier returned handles.
            workerSet(this, Math.max(workerGet(this), snapshot.turnId));
            return new AgentTurn(this, snapshot.turnId);
        }
        async status() { return currentTurn(this).status(); }
        async done() { return currentTurn(this).done(); }
        async output() { return currentTurn(this).output(); }
        async wait(timeout = 30) { return currentTurn(this).wait(timeout); }
        async close() { return currentTurn(this).close(); }
        async cancel() { return this.close(); }
    }
    const agentOptions = new Set([
        'agent', 'cwd', 'isolation', 'isoApply', 'outputSchema', 'schemaMode', 'timeoutSeconds', 'scope', 'keepAlive',
    ]);
    const hasAgentOption = agentOptions.has.bind(agentOptions);
    const launchAgent = async (prompt, options = {}) => {
        if (typeof prompt !== 'string' || !prompt.trim() || !options
            || typeof options !== 'object' || isArray(options)) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: agent requires a nonempty prompt and an options object');
        }
        const args = {agent:'task'};
        for (const name of keys(options)) {
            if (!hasAgentOption(name)) throw new TypeErrorType(`EVAL_AGENT_INPUT: unknown option ${name}`);
            if (name !== 'keepAlive') args[name] = options[name];
        }
        if (typeof args.agent !== 'string' || !args.agent) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: agent must be a nonempty name');
        }
        if (hasOwn(options, 'keepAlive') && typeof options.keepAlive !== 'boolean') {
            throw new TypeErrorType('EVAL_AGENT_INPUT: keepAlive must be boolean');
        }
        // Callers cannot overwrite the operation or turn a launch foreground.
        args.task = prompt;
        if (hasOwn(options, 'keepAlive') && options.keepAlive === true) {
            if ((hasOwn(args, 'isolation') && args.isolation !== 'none')
                || hasOwn(args, 'isoApply') || hasOwn(args, 'outputSchema') || hasOwn(args, 'schemaMode')) {
                throw new TypeErrorType('EVAL_AGENT_INPUT: warm workers require isolation none and do not accept schema or isoApply options');
            }
            args.operation = 'worker_start';
            const started = checkWorker(await nativeResult('subagent', args), undefined, 1);
            if (started.agent !== args.agent) throw new ErrorType('EVAL_AGENT_PROTOCOL: native worker agent changed');
            return new AgentWorker(started);
        }
        args.async = true;
        const started = await nativeResult('subagent', args);
        if (!started || started.schema !== 'pi.subagent.background.v1'
            || typeof started.jobId !== 'string' || !started.jobId) {
            throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid background acknowledgment');
        }
        return new AgentJob(started.jobId, args.agent);
    };
    globalThis.agent = launchAgent;
    globalThis.wait = async (handles, options = {}) => {
        if (!isArray(handles) || handles.length > 32 || handles.some(handle =>
            !(handle instanceof AgentJob) && !(handle instanceof AgentTurn) && !workerGet(handle))
            || !options || typeof options !== 'object' || isArray(options)
            || keys(options).some(name => name !== 'timeout' && name !== 'raiseErrors')) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: wait requires up to 32 agent handles and timeout/raiseErrors options');
        }
        const timeoutMs = waitMilliseconds(options.timeout === undefined ? 30 : options.timeout);
        const raiseErrors = options.raiseErrors === undefined ? true : options.raiseErrors;
        if (typeof raiseErrors !== 'boolean') throw new TypeErrorType('EVAL_AGENT_INPUT: raiseErrors must be boolean');
        const deadline = now() + timeoutMs;
        // Capture selection before the first await: another send cannot make
        // an already-started mixed wait target a different worker turn.
        const selected = handles.map(handle => workerGet(handle) ? currentTurn(handle) : handle);
        const results = [];
        for (const handle of selected) {
            const remaining = Math.max(0, deadline - now());
            results.push(await (handle instanceof AgentTurn
                ? waitTurn(handle, remaining, raiseErrors) : waitJob(handle, remaining, raiseErrors)));
        }
        return results;
    };
    const poolStates = new WeakMap();
    const poolGet = poolStates.get.bind(poolStates);
    const poolSet = poolStates.set.bind(poolStates);
    const boundedText = (value, maximum) => {
        if (typeof value !== 'string' || !value.trim() || value.includes('\0')) return false;
        let bytes = 0;
        for (let index = 0; index < value.length; index++) {
            const code = value.charCodeAt(index);
            if (code >= 0xd800 && code <= 0xdbff) {
                const next = value.charCodeAt(++index);
                if (!(next >= 0xdc00 && next <= 0xdfff)) return false;
                bytes += 4;
            } else if (code >= 0xdc00 && code <= 0xdfff) return false;
            else bytes += code < 0x80 ? 1 : code < 0x800 ? 2 : 3;
            if (bytes > maximum) return false;
        }
        return true;
    };
    const poolErrorText = error => error && typeof error.message === 'string' ? error.message : 'Pool operation failed';
    const serialPool = (state, action) => {
        if (state.busy && state.generation !== cellGeneration) {
            // A caller abandoned this operation at a preceding cell boundary.
            // Its callback was cleared; never wait forever on that old promise
            // or replay its possibly admitted work. Known handles remain usable.
            state.closed = true;
            state.interrupted = true;
            state.unknownAdmission ||= state.startPending;
            state.lastError = 'EVAL_POOL_INTERRUPTED: previous cell did not finish a pool command; admission closed';
            state.tail = resolvedPromise();
            state.busy = false;
        }
        const generation = cellGeneration;
        const pending = promiseThen(state.tail, async () => {
            if (generation !== cellGeneration) throw new ErrorType('EVAL_POOL_INTERRUPTED: queued command belongs to a completed cell');
            state.busy = true;
            state.generation = generation;
            try { return await action(); }
            finally { if (state.generation === generation) state.busy = false; }
        });
        state.tail = promiseThen(pending, () => undefined, () => undefined);
        return pending;
    };
    const peekPool = async state => {
        const items = [];
        for (let index = 0; index < state.items.length; index++) {
            const handle = state.items[index];
            const identity = {index,workerId:handle.workerId,jobId:handle.jobId,turnId:handle.turnId};
            try {
                const snapshot = await handle.status();
                items.push({...identity,status:snapshot.turn.status,result:snapshot.turn.result,workerStatus:snapshot.status});
            } catch (error) {
                items.push({...identity,status:'unavailable',result:null,error:poolErrorText(error)});
            }
        }
        return {schema:'pi.eval.workpool.v1',name:state.name,closed:state.closed,
            interrupted:state.interrupted,unknownAdmission:state.unknownAdmission,
            concurrency:state.concurrency,workerCount:state.workers.length,
            items,lastError:state.lastError};
    };
    class AgentPool {
        constructor(options, name, concurrency) {
            poolSet(this,{options,name,concurrency,workers:[],items:[],closed:false,interrupted:false,
                unknownAdmission:false,startPending:false,lastError:null,
                tail:resolvedPromise(),busy:false,generation:cellGeneration});
            freeze(this);
        }
        get name() { return poolGet(this).name; }
        get concurrency() { return poolGet(this).concurrency; }
        get closed() { return poolGet(this).closed; }
        get items() { return freeze([...poolGet(this).items]); }
        async push(tasks) {
            if (!isArray(tasks) || tasks.length > 32) {
                throw new TypeErrorType('EVAL_POOL_INPUT: push requires an array of up to 32 task strings');
            }
            const batch = [...tasks];
            if (batch.some(task => !boundedText(task,65536))) {
                throw new TypeErrorType('EVAL_POOL_INPUT: every task must be nonempty UTF-8 text of at most 65536 bytes without NUL');
            }
            const state = poolGet(this);
            return serialPool(state, async () => {
                if (state.closed) throw new ErrorType('EVAL_POOL_CLOSED: pool admission is closed; inspect items or close existing workers');
                if (state.items.length + batch.length > 32) throw new RangeErrorType('EVAL_POOL_CAPACITY: at most 32 items may be admitted per pool');
                if (batch.length === 0) return [];
                const admitted = [];
                try {
                    // Refresh each existing worker once, not once per item.
                    // Newly admitted native turns run after this cell returns.
                    for (const entry of state.workers) entry.status = (await entry.worker.status()).status;
                    for (const task of batch) {
                        let chosen = state.workers.filter(entry => entry.status === 'idle')
                            .sort((a,b) => a.admitted - b.admitted)[0];
                        let handle;
                        if (!chosen && state.workers.length < state.concurrency) {
                            state.startPending = true;
                            const worker = await launchAgent(task,state.options);
                            chosen = {worker,admitted:0,status:'running',closeAcknowledged:false};
                            state.workers.push(chosen);
                            state.startPending = false;
                            handle = currentTurn(worker);
                        } else {
                            if (!chosen) chosen = state.workers
                                .filter(entry => entry.status === 'running' || entry.status === 'starting')
                                .sort((a,b) => a.admitted - b.admitted)[0];
                            if (!chosen) throw new ErrorType('EVAL_POOL_UNAVAILABLE: no live worker remains; create a new pool');
                            handle = await chosen.worker.send(task);
                        }
                        chosen.admitted++;
                        chosen.status = 'running';
                        state.items.push(handle);
                        admitted.push(handle);
                    }
                    state.lastError = null;
                    return admitted;
                } catch (cause) {
                    // A failed acknowledgment may follow a native side effect.
                    // Close admission instead of silently retrying the batch.
                    state.closed = true;
                    state.unknownAdmission ||= state.startPending;
                    state.startPending = false;
                    state.lastError = poolErrorText(cause);
                    const error = new ErrorType(`EVAL_POOL_ADMISSION: acknowledged ${admitted.length} of ${batch.length} items; admission closed. Handles remain in pool.items and peek(). ${state.lastError}`);
                    error.admitted = freeze([...admitted]);
                    error.requested = batch.length;
                    error.cause = cause;
                    throw error;
                }
            });
        }
        async peek() {
            const state = poolGet(this);
            return serialPool(state, () => peekPool(state));
        }
        async status() {
            const state = poolGet(this);
            return serialPool(state, async () => {
                const snapshot = await peekPool(state);
                const counts = {queued:0,running:0,completed:0,failed:0,cancelled:0,unavailable:0};
                for (const item of snapshot.items) counts[item.status]++;
                return {schema:snapshot.schema,name:snapshot.name,closed:snapshot.closed,
                    interrupted:snapshot.interrupted,unknownAdmission:snapshot.unknownAdmission,
                    concurrency:snapshot.concurrency,
                    workerCount:snapshot.workerCount,total:snapshot.items.length,counts,lastError:snapshot.lastError};
            });
        }
        async close() {
            const state = poolGet(this);
            return serialPool(state, async () => {
                state.closed = true;
                const workers = [];
                for (const entry of state.workers) {
                    if (!entry.closeAcknowledged) {
                        try {
                            const snapshot = await entry.worker.close();
                            entry.status = snapshot.status;
                            entry.closeAcknowledged = snapshot.status === 'closed' || snapshot.status === 'failed';
                            entry.closeError = entry.closeAcknowledged ? null : 'EVAL_POOL_CLOSE: worker did not acknowledge termination';
                        } catch (error) { entry.closeError = poolErrorText(error); }
                    }
                    workers.push({workerId:entry.worker.id,jobId:entry.worker.jobId,
                        closed:entry.closeAcknowledged,error:entry.closeError ?? null});
                }
                const allKnownClosed = workers.every(worker => worker.closed);
                return {schema:'pi.eval.workpool.close.v1',name:state.name,closed:true,
                    allClosed:allKnownClosed && !state.unknownAdmission,allKnownClosed,
                    unknownAdmission:state.unknownAdmission,workers};
            });
        }
    }
    globalThis.workpool = async (options = {}) => {
        if (!options || typeof options !== 'object' || isArray(options)) throw new TypeErrorType('EVAL_POOL_INPUT: workpool requires an options object');
        const native = {agent:'task',keepAlive:true};
        let name = 'workpool';
        let concurrency = 4;
        for (const key of keys(options)) {
            const value = options[key];
            if (key === 'name') {
                if (!boundedText(value,128)) throw new TypeErrorType('EVAL_POOL_INPUT: name must be nonempty text of at most 128 UTF-8 bytes without NUL');
                name = value;
            } else if (key === 'concurrency') {
                if (!integer(value) || value < 1 || value > 8) throw new RangeErrorType('EVAL_POOL_INPUT: concurrency must be an integer from 1 to 8');
                concurrency = value;
            } else if (key === 'agent') {
                if (!boundedText(value,128)) throw new TypeErrorType('EVAL_POOL_INPUT: agent must be nonempty text of at most 128 UTF-8 bytes without NUL');
                native.agent = value;
            } else if (key === 'cwd') {
                if (typeof value !== 'string' || !value || value.includes('\0')) throw new TypeErrorType('EVAL_POOL_INPUT: cwd must be a nonempty path without NUL');
                native.cwd = value;
            } else if (key === 'scope') {
                if (value !== 'user' && value !== 'project' && value !== 'both') throw new TypeErrorType('EVAL_POOL_INPUT: scope must be user, project or both');
                native.scope = value;
            } else if (key === 'isolation') {
                if (value !== 'none') throw new TypeErrorType('EVAL_POOL_INPUT: workpool requires isolation none');
                native.isolation = value;
            } else if (key === 'timeoutSeconds') {
                if (!integer(value) || value < 1 || value > 86400) throw new RangeErrorType('EVAL_POOL_INPUT: timeoutSeconds must be an integer from 1 to 86400');
                native.timeoutSeconds = value;
            } else throw new TypeErrorType(`EVAL_POOL_INPUT: unknown option ${key}`);
        }
        return new AgentPool(freeze(native),name,concurrency);
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
            cellGeneration++;
        },
    };
})(__pi_set_timer, __pi_clear_timer, __pi_async_call);
