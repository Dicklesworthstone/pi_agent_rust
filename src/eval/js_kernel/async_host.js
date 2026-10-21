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
            !(handle instanceof AgentJob) && !(handle instanceof AgentTurn)
            && !(handle instanceof AgentPoolItem) && !workerGet(handle))
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
            results.push(await (handle instanceof AgentTurn ? waitTurn(handle, remaining, raiseErrors)
                : handle instanceof AgentPoolItem ? waitPoolItem(handle, remaining, raiseErrors)
                : waitJob(handle, remaining, raiseErrors)));
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
    const poolStatuses = new Set(['open', 'draining', 'completed', 'failed', 'cancelled']);
    const validPoolStatus = poolStatuses.has.bind(poolStatuses);
    const poolTerminal = status => status !== 'open' && status !== 'draining';
    const checkPool = (snapshot, poolId, itemId) => {
        if (!snapshot || snapshot.schema !== 'pi.subagent.pool.v1'
            || typeof snapshot.poolId !== 'string' || !snapshot.poolId
            || (poolId !== undefined && snapshot.poolId !== poolId)
            || typeof snapshot.jobId !== 'string' || !snapshot.jobId
            || typeof snapshot.agent !== 'string' || !snapshot.agent
            || !validPoolStatus(snapshot.status) || typeof snapshot.sealed !== 'boolean'
            || !integer(snapshot.concurrency) || snapshot.concurrency < 1 || snapshot.concurrency > 8
            || !isArray(snapshot.items) || snapshot.items.length > 32
            || snapshot.admittedItems !== snapshot.items.length
            || !integer(snapshot.remainingMs) || snapshot.remainingMs < 0
            || ['queuedItems','runningItems','completedItems','failedItems'].some(key =>
                !integer(snapshot[key]) || snapshot[key] < 0 || snapshot[key] > snapshot.admittedItems)) {
            throw new ErrorType('EVAL_POOL_PROTOCOL: invalid native pool snapshot');
        }
        const counts = {queued:0,running:0,completed:0,failed:0,cancelled:0};
        for (let index = 0; index < snapshot.items.length; index++) {
            const item = snapshot.items[index];
            if (!item || item.id !== index + 1 || !validTurnStatus(item.status)
                || typeof item.summary !== 'string' || !item.resultRef
                || item.resultRef.poolId !== snapshot.poolId || item.resultRef.itemId !== item.id) {
                throw new ErrorType('EVAL_POOL_PROTOCOL: invalid native pool item manifest');
            }
            counts[item.status]++;
        }
        if (snapshot.queuedItems !== counts.queued || snapshot.runningItems !== counts.running
            || snapshot.completedItems !== counts.completed || snapshot.failedItems !== counts.failed + counts.cancelled
            || (snapshot.status === 'open') === snapshot.sealed) {
            throw new ErrorType('EVAL_POOL_PROTOCOL: inconsistent native pool state');
        }
        if (itemId !== undefined || hasOwn(snapshot, 'itemId') || hasOwn(snapshot, 'item')) {
            const item = snapshot.item;
            if (!validTurnId(snapshot.itemId) || (itemId !== undefined && snapshot.itemId !== itemId)
                || !item || item.id !== snapshot.itemId || !validTurnStatus(item.status)
                || snapshot.items[item.id - 1]?.status !== item.status || !hasOwn(item, 'result')) {
                throw new ErrorType('EVAL_POOL_PROTOCOL: invalid selected pool item');
            }
            const result = item.result;
            if ((result === null && turnDone(item)) || (result !== null
                && (!result || !isArray(result.content) || !hasOwn(result, 'details')
                    || (hasOwn(result, 'isError') && typeof result.isError !== 'boolean')))) {
                throw new ErrorType('EVAL_POOL_PROTOCOL: invalid native pool item result');
            }
        }
        return snapshot;
    };
    const readPoolItem = async (handle, operation, timeoutMs) => {
        const args = {operation,poolId:handle.poolId,itemId:handle.itemId};
        if (timeoutMs !== undefined) args.timeoutMs = timeoutMs;
        const snapshot = checkPool(await nativeResult('subagent',args),handle.poolId,handle.itemId);
        if (snapshot.jobId !== handle.jobId || snapshot.agent !== handle.agent) {
            throw new ErrorType('EVAL_POOL_PROTOCOL: native pool identity changed');
        }
        return snapshot;
    };
    const waitPoolItem = async (handle, timeoutMs, raiseErrors) => {
        const snapshot = await readPoolItem(handle,'pool_wait',timeoutMs);
        if (!turnDone(snapshot.item)) {
            const error = new ErrorType('EVAL_AGENT_TIMEOUT: pool item is still pending; the native pool remains available');
            error.name = 'TimeoutError';
            throw error;
        }
        const result = snapshot.item.result;
        if (raiseErrors && (snapshot.item.status !== 'completed' || result.isError)) {
            throw new ErrorType(resultText(result) || `Pool item ${snapshot.item.status}`);
        }
        return result;
    };
    class AgentPoolItem {
        constructor(pool, itemId) {
            this.id = itemId;
            this.itemId = itemId;
            this.poolId = pool.poolId;
            this.jobId = pool.jobId;
            this.agent = pool.agent;
            freeze(this);
        }
        async status() { return readPoolItem(this,'pool_result'); }
        async done() { return turnDone((await this.status()).item); }
        async output() { return (await this.status()).item.result; }
        async wait(timeout = 30) { return waitPoolItem(this,waitMilliseconds(timeout),true); }
        // Item handles cannot close a shared carrier. Use pool.close() to
        // explicitly cancel every running and queued item in the pool.
    }
    const serialPool = (state, action) => {
        if (state.busy && state.generation !== cellGeneration) {
            // A caller abandoned this operation at a preceding cell boundary.
            // Its callback was cleared; never wait forever on that old promise
            // or replay its possibly admitted work. Known handles remain usable.
            state.closed = true;
            state.interrupted = true;
            state.unknownAdmission ||= state.admissionPending;
            state.admissionPending = false;
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
    const adoptPool = (state, snapshot) => {
        checkPool(snapshot,state.poolId ?? undefined);
        if (snapshot.agent !== state.options.agent || snapshot.concurrency !== state.concurrency
            || (state.jobId !== null && snapshot.jobId !== state.jobId)
            || snapshot.items.length < state.items.length) {
            throw new ErrorType('EVAL_POOL_PROTOCOL: native pool identity or retained items changed');
        }
        state.poolId = snapshot.poolId;
        state.jobId = snapshot.jobId;
        for (let index = state.items.length; index < snapshot.items.length; index++) {
            state.items.push(new AgentPoolItem(snapshot,index + 1));
        }
        state.snapshot = snapshot;
        state.closed ||= snapshot.sealed || poolTerminal(snapshot.status);
        state.unknownAdmission = false;
        return snapshot;
    };
    const poolView = state => ({
        ...(state.snapshot ? {...state.snapshot,items:state.snapshot.items.map(item =>
            ({...item,resultRef:{...item.resultRef}}))} : {
            schema:'pi.eval.workpool.pending.v1',poolId:null,jobId:null,agent:state.options.agent,
            status:state.unknownAdmission ? 'unavailable' : state.closed ? 'cancelled' : 'unstarted',
            sealed:state.closed,concurrency:state.concurrency,admittedItems:0,items:[],
        }),
        name:state.name,closed:state.closed,interrupted:state.interrupted,
        unknownAdmission:state.unknownAdmission,lastError:state.lastError,
    });
    const poolCommand = async (state, operation, extra = {}) => adoptPool(state,
        await nativeResult('subagent',{operation,poolId:state.poolId,...extra}));
    const poolBatchArgs = (state, tasks) => {
        const args = state.poolId === null
            ? {...state.options,operation:'pool_start',concurrency:state.concurrency,tasks}
            : {operation:'pool_push',poolId:state.poolId,tasks};
        // The JS bridge admits at most 256 KiB per encoded request. Keep the
        // same guest contract in Python and reject before a native admission.
        if (!boundedText(stringify({name:'subagent',args}),256 * 1024)) {
            throw new RangeErrorType('EVAL_POOL_INPUT: encoded batch exceeds 256 KiB; split tasks across smaller push() calls');
        }
        return args;
    };
    const peekPool = async state => {
        if (state.poolId !== null) await poolCommand(state,'pool_status');
        return poolView(state);
    };
    class AgentPool {
        constructor(options, name, concurrency) {
            poolSet(this,{options,name,concurrency,poolId:null,jobId:null,snapshot:null,items:[],
                closed:false,interrupted:false,finishAcknowledged:false,closeAcknowledged:false,
                unknownAdmission:false,admissionPending:false,lastError:null,
                tail:resolvedPromise(),busy:false,generation:cellGeneration});
            freeze(this);
        }
        get id() { return poolGet(this).poolId; }
        get poolId() { return poolGet(this).poolId; }
        get jobId() { return poolGet(this).jobId; }
        get name() { return poolGet(this).name; }
        get concurrency() { return poolGet(this).concurrency; }
        get closed() { return poolGet(this).closed; }
        get unknownAdmission() { return poolGet(this).unknownAdmission; }
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
                const args = poolBatchArgs(state,batch);
                try {
                    state.admissionPending = true;
                    const previous = state.items.length;
                    const snapshot = adoptPool(state,await nativeResult('subagent',args));
                    const ids = snapshot.admittedItemIds;
                    if (!isArray(ids) || ids.length !== batch.length || ids.some((id,index) =>
                        !validTurnId(id) || id <= previous || id > state.items.length
                        || (index > 0 && id !== ids[index - 1] + 1))) {
                        throw new ErrorType('EVAL_POOL_PROTOCOL: invalid atomic batch acknowledgment');
                    }
                    state.lastError = null;
                    return ids.map(id => state.items[id - 1]);
                } catch (cause) {
                    // Admission is native and atomic. A lost acknowledgment
                    // must never cause replay; a known pool can be reconciled
                    // by peek(), including all items admitted by that request.
                    state.closed = true;
                    state.unknownAdmission = true;
                    state.lastError = poolErrorText(cause);
                    const error = new ErrorType(`EVAL_POOL_ADMISSION: batch acknowledgment failed; admission closed without retry. Retained handles remain in pool.items; peek() can reconcile a known pool. ${state.lastError}`);
                    error.admitted = freeze([...state.items]);
                    error.requested = batch.length;
                    error.cause = cause;
                    throw error;
                } finally {
                    state.admissionPending = false;
                }
            });
        }
        async peek() {
            const state = poolGet(this);
            return serialPool(state, () => peekPool(state));
        }
        async status() {
            const state = poolGet(this);
            return serialPool(state, () => peekPool(state));
        }
        async finish() {
            const state = poolGet(this);
            return serialPool(state, async () => {
                if (state.closeAcknowledged || (state.poolId === null && state.closed)) {
                    throw new ErrorType('EVAL_POOL_CLOSED: a cancelled or unacknowledged pool cannot be finished');
                }
                state.closed = true;
                if (!state.finishAcknowledged) {
                    try {
                        if (state.poolId === null) {
                            const args = poolBatchArgs(state,[]);
                            state.admissionPending = true;
                            adoptPool(state,await nativeResult('subagent',args));
                            state.admissionPending = false;
                        }
                        const snapshot = await poolCommand(state,'pool_finish');
                        if (!snapshot.sealed) throw new ErrorType('EVAL_POOL_PROTOCOL: pool finish did not seal admission');
                        state.finishAcknowledged = true;
                    } catch (error) {
                        state.unknownAdmission ||= state.admissionPending;
                        state.lastError = poolErrorText(error);
                        throw error;
                    } finally { state.admissionPending = false; }
                }
                return new AgentJob(state.jobId,state.options.agent);
            });
        }
        async wait(timeout = 30) {
            const deadline = now() + waitMilliseconds(timeout);
            const job = await this.finish();
            return waitJob(job,Math.max(0,deadline - now()),true);
        }
        async output() {
            const state = poolGet(this);
            return serialPool(state, async () => state.jobId === null ? null
                : (await readJob(state.jobId,'wait',0)).result ?? null);
        }
        async close() {
            const state = poolGet(this);
            return serialPool(state, async () => {
                state.closed = true;
                if (state.poolId !== null && !state.closeAcknowledged) {
                    try {
                        const snapshot = await poolCommand(state,'pool_close');
                        if (!poolTerminal(snapshot.status)) throw new ErrorType('EVAL_POOL_PROTOCOL: pool close did not acknowledge termination');
                        state.closeAcknowledged = true;
                    } catch (error) {
                        state.lastError = poolErrorText(error);
                        throw error;
                    }
                }
                const allKnownClosed = state.poolId === null || state.closeAcknowledged;
                return {...poolView(state),closed:true,
                    allClosed:allKnownClosed && !state.unknownAdmission,allKnownClosed,
                    unknownAdmission:state.unknownAdmission};
            });
        }
        async cancel() { return this.close(); }
    }
    globalThis.workpool = async (options = {}) => {
        if (!options || typeof options !== 'object' || isArray(options)) throw new TypeErrorType('EVAL_POOL_INPUT: workpool requires an options object');
        const native = {agent:'task'};
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
        poolBatchArgs({options:native,poolId:null,concurrency},[]);
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
