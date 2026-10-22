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
    const drainOwnKeys = Reflect.ownKeys;
    const drainDescriptor = Object.getOwnPropertyDescriptor;
    const drainCreate = Object.create;
    const drainPrototype = Object.getPrototypeOf;
    const drainPlainPrototype = Object.prototype;
    const drainSetPrototype = Object.setPrototypeOf;
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
    // Model jobs retain only native identity. Input privacy/model resolution is
    // completed by the host before acknowledgment; later observations re-enter
    // the current host instead of caching results or launch-time authority.
    const modelOperations = freeze({
        completion:'__pi_model_complete_start', judgment:'__pi_judge_start',
        judgment_batch:'__pi_judge_batch_start',
    });
    const modelJobId = id => typeof id === 'string' && id.length > 0 && id.length <= 128
        && !/[\s\x00-\x1f\x7f]/.test(id);
    const modelRecord = value => value !== null && typeof value === 'object' && !isArray(value);
    const modelProtocol = () => new ErrorType('EVAL_MODEL_PROTOCOL: invalid native model job response');
    const modelPayload = (operation, payload) => {
        if (!modelRecord(payload)) throw modelProtocol();
        if (operation === 'completion') {
            if (payload.schema !== 'pi.completion.v1' || typeof payload.text !== 'string'
                || (hasOwn(payload,'data') && payload.schemaValid !== true)
                || (hasOwn(payload,'schemaValid') && (!hasOwn(payload,'data') || payload.schemaValid !== true))) {
                throw modelProtocol();
            }
        } else if (operation === 'judgment') {
            if (payload.schema !== 'pi.judgment.v1' || !modelRecord(payload.answers)
                || keys(payload.answers).length === 0) throw modelProtocol();
        } else {
            if (payload.schema !== 'pi.eval.judgment-batch.v1' || !isArray(payload.items)
                || !integer(payload.total) || payload.total !== payload.items.length || payload.total < 1
                || !integer(payload.succeeded) || payload.succeeded < 0
                || !integer(payload.failed) || payload.failed < 0
                || payload.succeeded + payload.failed !== payload.total
                || payload.allOk !== (payload.failed === 0)) throw modelProtocol();
            let succeeded = 0;
            for (const item of payload.items) {
                if (!modelRecord(item) || typeof item.ok !== 'boolean'
                    || !(typeof item.key === 'string' && item.key.length > 0
                        || integer(item.key) && item.key >= 0)) throw modelProtocol();
                if (item.ok) { modelPayload('judgment',item.verdict); succeeded++; }
                else if (typeof item.error !== 'string' || !item.error) throw modelProtocol();
            }
            if (succeeded !== payload.succeeded) throw modelProtocol();
        }
        return payload;
    };
    const checkModelProgress = (job, result, wrapperSchema) => {
        if (job.operation !== 'judgment_batch' || !modelRecord(result) || !isArray(result.content)
            || (hasOwn(result,'isError') && typeof result.isError !== 'boolean')
            || !modelRecord(result.details) || result.details.schema !== wrapperSchema) throw modelProtocol();
        const partial = result.details.partialResult;
        if (!modelRecord(partial) || !isArray(partial.content)
            || (hasOwn(partial,'isError') && partial.isError !== false)
            || !modelRecord(partial.details)) throw modelProtocol();
        const progress = partial.details;
        if (progress.schema !== 'pi.eval.model-job.progress.v1' || progress.operation !== 'judgment_batch'
            || progress.complete !== false || !integer(progress.total) || progress.total < 1 || progress.total > 16
            || !isArray(progress.items) || progress.items.length !== progress.total
            || ['completed','succeeded','failed','queued','running'].some(name =>
                !integer(progress[name]) || progress[name] < 0 || progress[name] > progress.total)
            || progress.completed !== progress.succeeded + progress.failed
            || progress.total !== progress.completed + progress.queued + progress.running
            || !modelRecord(progress.judgments) || progress.judgments.schema !== 'pi.eval.judgments.v1'
            || !isArray(progress.judgments.attempts) || progress.judgments.attempts.length !== progress.total
            || progress.judgments.attempts.some(receipt => !modelRecord(receipt))) throw modelProtocol();
        const counts = {queued:0,running:0,completed:0,failed:0};
        const seen = [];
        for (const item of progress.items) {
            if (!modelRecord(item) || typeof item.status !== 'string' || !hasOwn(counts,item.status)
                || !(typeof item.key === 'string' && item.key.length > 0 || integer(item.key) && item.key >= 0)
                || seen.some(key => key === item.key)) throw modelProtocol();
            seen.push(item.key);
            counts[item.status]++;
            if (item.status === 'completed') {
                if (hasOwn(item,'error')) throw modelProtocol();
                modelPayload('judgment',item.verdict);
            } else if (item.status === 'failed') {
                if (hasOwn(item,'verdict') || typeof item.error !== 'string' || !item.error) throw modelProtocol();
            } else if (hasOwn(item,'verdict') || hasOwn(item,'error')) throw modelProtocol();
        }
        if (counts.queued !== progress.queued || counts.running !== progress.running
            || counts.completed !== progress.succeeded || counts.failed !== progress.failed) throw modelProtocol();
        return progress;
    };
    const modelProgressView = (job, snapshot) => {
        const details = snapshot.result?.details;
        if (!details) return null;
        if (details.schema === 'pi.host_job.progress.v1' || details.schema === 'pi.host_job.interrupted.v1') {
            return {...details.partialResult.details,jobStatus:snapshot.status};
        }
        if (details.result === null) return null;
        const batch = details.result;
        return {schema:'pi.eval.model-job.progress.v1',operation:job.operation,complete:true,jobStatus:snapshot.status,
            total:batch.total,completed:batch.total,succeeded:batch.succeeded,failed:batch.failed,queued:0,running:0,
            items:batch.items.map(item => item.ok ? {key:item.key,status:'completed',verdict:item.verdict}
                : {key:item.key,status:'failed',error:item.error}),judgments:details.judgments};
    };
    const checkModelJob = (job, snapshot) => {
        const expectedLabel = job.operation === 'completion' ? 'eval model completion' : `eval ${job.operation}`;
        if (snapshot.command !== expectedLabel
            || !['running','exited','failed','killed','timedOut'].includes(snapshot.status)) throw modelProtocol();
        if (snapshot.status === 'running') {
            if (snapshot.result !== undefined && snapshot.result !== null) {
                checkModelProgress(job,snapshot.result,'pi.host_job.progress.v1');
                if (snapshot.result.isError) throw modelProtocol();
            }
            return snapshot;
        }
        const result = snapshot.result;
        if (!modelRecord(result) || !isArray(result.content) || !hasOwn(result,'details')
            || (hasOwn(result,'isError') && typeof result.isError !== 'boolean')) throw modelProtocol();
        const details = result.details;
        if (details === null) {
            // Native cancellation/timeout can retire work before a typed
            // result exists. Preserve that failure; never invent a verdict.
            if (snapshot.status === 'exited' || !result.isError) throw modelProtocol();
        } else if (modelRecord(details) && details.schema === 'pi.host_job.interrupted.v1') {
            if ((snapshot.status !== 'killed' && snapshot.status !== 'timedOut') || !result.isError) {
                throw modelProtocol();
            }
            checkModelProgress(job,result,'pi.host_job.interrupted.v1');
        } else {
            if (!modelRecord(details) || details.schema !== 'pi.eval.model-job.result.v1'
                || details.operation !== job.operation || !hasOwn(details,'result')
                || !modelRecord(details.judgments) || details.judgments.schema !== 'pi.eval.judgments.v1'
                || !isArray(details.judgments.attempts)) throw modelProtocol();
            if (details.result === null) {
                if (!result.isError || typeof details.error !== 'string' || !details.error) throw modelProtocol();
            } else {
                const payload = modelPayload(job.operation,details.result);
                if (job.operation === 'judgment_batch' && (result.isError === true) !== !payload.allOk) throw modelProtocol();
                if (job.operation !== 'judgment_batch' && result.isError) throw modelProtocol();
            }
            if (snapshot.status !== 'exited' && !result.isError) throw modelProtocol();
        }
        return snapshot;
    };
    const readModelJob = async (job, action, timeoutMs) =>
        checkModelJob(job,await readJob(job.id,action,timeoutMs));
    const waitModelJob = async (job, timeoutMs, raiseErrors) => {
        const snapshot = await readModelJob(job,'wait',timeoutMs);
        if (snapshot.status === 'running') {
            const error = new ErrorType('EVAL_MODEL_TIMEOUT: model job is still running; retain its id and wait again');
            error.name = 'TimeoutError';
            throw error;
        }
        const result = snapshot.result;
        if (raiseErrors && (snapshot.status !== 'exited' || result.isError)) {
            const error = new ErrorType(resultText(result) || `Model job ${snapshot.status}`);
            error.result = result;
            error.jobId = job.id;
            throw error;
        }
        const payload = result.details?.schema === 'pi.eval.model-job.result.v1'
            ? result.details.result : undefined;
        if (payload === null || payload === undefined) return result;
        if (job.operation === 'completion') return hasOwn(payload,'data') ? payload.data : payload.text;
        return job.operation === 'judgment' ? payload.answers : payload;
    };
    // A drain has no hidden consuming cursor. The caller acknowledges a page
    // only after processing it, by supplying page.cursor to a later drain.
    // Use the native waiter: guest polling cannot recheck policy at native
    // readiness boundaries or preserve its host-call/deadline accounting.
    const drainInput = () => new TypeErrorType('EVAL_BATCH_INPUT: drain accepts cursor and timeout seconds from 0 through 600');
    const drainProtocol = () => new ErrorType('EVAL_BATCH_PROTOCOL: invalid native drain response');
    const drainRecord = (value, allowed, fail) => {
        if (!modelRecord(value) || (drainPrototype(value) !== drainPlainPrototype && drainPrototype(value) !== null)) throw fail();
        const copied = drainCreate(null);
        for (const key of drainOwnKeys(value)) {
            const field = drainDescriptor(value,key);
            if (typeof key !== 'string' || !allowed.includes(key) || !field || !hasOwn(field,'value')) throw fail();
            copied[key] = field.value;
        }
        return copied;
    };
    const drainCursor = (value, id, fail) => {
        if (value === null) return null;
        const cursor = drainRecord(value,['schema','jobId','manifest','seen'],fail);
        if (keys(cursor).length !== 4 || cursor.schema !== 'pi.eval.batch-cursor.v1' || cursor.jobId !== id
            || !isArray(cursor.seen) || cursor.seen.length > 16
            || (cursor.manifest !== null && (typeof cursor.manifest !== 'string' || !/^[0-9a-f]{64}$/.test(cursor.manifest)))) throw fail();
        const seen = [];
        for (let index = 0; index < cursor.seen.length; index++) {
            const field = drainDescriptor(cursor.seen,String(index));
            if (!field || !hasOwn(field,'value') || !integer(field.value) || field.value < 0 || field.value >= 16
                || seen.includes(field.value)) throw fail();
            seen.push(field.value);
        }
        if (drainOwnKeys(cursor.seen).length !== seen.length + 1 || (seen.length && cursor.manifest === null)) throw fail();
        seen.sort((left,right) => left - right);
        // Prevent an inherited toJSON hook from rewriting a checked cursor.
        drainSetPrototype(seen,null);
        cursor.seen = seen;
        return cursor;
    };
    const drainHas = (seen, index) => {
        for (let offset = 0; offset < seen.length; offset++) if (seen[offset] === index) return true;
        return false;
    };
    const checkDrainPage = (page, id, previous) => {
        if (!modelRecord(page) || page.schema !== 'pi.eval.batch-drain.v1' || page.jobId !== id
            || !['running','exited','failed','killed','timedOut'].includes(page.jobStatus)
            || page.done !== (page.jobStatus !== 'running')
            || page.jobFailed !== (page.jobStatus !== 'running' && page.jobStatus !== 'exited')
            || typeof page.complete !== 'boolean' || typeof page.evidenceAvailable !== 'boolean'
            || !isArray(page.items) || page.items.length > 16) throw drainProtocol();
        const cursor = drainCursor(page.cursor,id,drainProtocol);
        if (cursor === null) throw drainProtocol();
        const before = previous === null ? [] : previous.seen;
        const manifest = previous === null ? null : previous.manifest;
        if (manifest !== null && cursor.manifest !== manifest) throw drainProtocol();
        if (!page.evidenceAvailable) {
            if (page.total !== null || page.settled !== null || page.complete || page.items.length
                || page.jobStatus === 'exited' || cursor.manifest !== manifest || cursor.seen.length !== before.length) throw drainProtocol();
            for (let index = 0; index < before.length; index++) if (!drainHas(cursor.seen,before[index])) throw drainProtocol();
            return page;
        }
        if (!integer(page.total) || page.total < 1 || page.total > 16 || !integer(page.settled)
            || page.settled < 0 || page.settled > page.total || cursor.manifest === null
            || (page.complete ? !page.done || page.settled !== page.total
                : !['running','killed','timedOut'].includes(page.jobStatus))) throw drainProtocol();
        for (let index = 0; index < cursor.seen.length; index++) if (cursor.seen[index] >= page.total) throw drainProtocol();
        const added = [];
        const itemKeys = [];
        let lastIndex = -1;
        let keyType = null;
        for (const item of page.items) {
            if (!modelRecord(item) || !integer(item.index) || item.index <= lastIndex || item.index >= page.total
                || drainHas(before,item.index) || typeof item.ok !== 'boolean'
                || !(typeof item.key === 'string' && item.key.length > 0 && item.key.length <= 128
                    || integer(item.key) && item.key === item.index)
                || itemKeys.some(key => key === item.key) || (keyType !== null && keyType !== typeof item.key)) throw drainProtocol();
            lastIndex = item.index;
            keyType = typeof item.key;
            itemKeys.push(item.key);
            added.push(item.index);
            if (item.ok) {
                if (hasOwn(item,'error')) throw drainProtocol();
                try { modelPayload('judgment',item.verdict); } catch (_) { throw drainProtocol(); }
            } else if (hasOwn(item,'verdict') || typeof item.error !== 'string' || !item.error
                || page.jobStatus === 'exited') throw drainProtocol();
        }
        if (before.length + added.length !== page.settled || cursor.seen.length !== page.settled) throw drainProtocol();
        for (let index = 0; index < before.length; index++) if (!drainHas(cursor.seen,before[index])) throw drainProtocol();
        for (const index of added) if (!drainHas(cursor.seen,index)) throw drainProtocol();
        return page;
    };
    const drainModelJob = async (job, options) => {
        if (job.operation !== 'judgment_batch' || !modelJobId(job.id)) throw drainInput();
        const fields = drainRecord(options,['cursor','timeout'],drainInput);
        const seconds = hasOwn(fields,'timeout') ? fields.timeout : 30;
        if (typeof seconds !== 'number' || !finite(seconds) || seconds < 0 || seconds > 600) throw drainInput();
        const cursor = drainCursor(hasOwn(fields,'cursor') ? fields.cursor : null,job.id,drainInput);
        const args = drainCreate(null);
        args.jobId = job.id;
        args.cursor = cursor;
        args.timeoutMs = Math.floor(seconds * 1000);
        const encoded = await callTool('__pi_judge_batch_drain',args,false);
        let page;
        try { page = parse(encoded); } catch (_) { throw drainProtocol(); }
        return checkDrainPage(page,job.id,cursor);
    };
    class ModelJob {
        constructor(id, operation) {
            this.id = id;
            this.jobId = id;
            this.operation = operation;
            freeze(this);
        }
        async status() { return readModelJob(this,'wait',0); }
        async done() { return (await this.status()).status !== 'running'; }
        async output() { return (await this.status()).result ?? null; }
        async progress() {
            if (this.operation !== 'judgment_batch') {
                throw new TypeErrorType('EVAL_MODEL_INPUT: progress is available only for judgment batches');
            }
            return modelProgressView(this,await this.status());
        }
        async drain(options = {}) { return drainModelJob(this,options); }
        async cancel() { return readModelJob(this,'cancel'); }
        async wait(timeout = 30, raiseErrors = true) {
            if (typeof raiseErrors !== 'boolean') throw new TypeErrorType('EVAL_MODEL_INPUT: raiseErrors must be boolean');
            return waitModelJob(this,waitMilliseconds(timeout),raiseErrors);
        }
    }
    const modelRoles = freeze({default:true,smol:true,slow:true,plan:true,commit:true,
        vision:true,designer:true,task:true,advisor:true,tiny:true});
    const modelOptions = (options, allowed) => {
        if (!modelRecord(options)) throw new TypeErrorType('EVAL_MODEL_INPUT: options must be an object');
        const args = {};
        for (const key of keys(options)) {
            if (!hasOwn(allowed,key)) throw new TypeErrorType(`EVAL_MODEL_INPUT: unknown option ${key}`);
            const value = options[key];
            if (key === 'model' && (typeof value !== 'string' || !hasOwn(modelRoles,value))) {
                throw new TypeErrorType('EVAL_MODEL_INPUT: model must name a canonical native role');
            }
            if (key === 'timeoutSeconds' && (!integer(value) || value < 1 || value > 600)) {
                throw new RangeErrorType('EVAL_MODEL_INPUT: timeoutSeconds must be an integer from 1 to 600');
            }
            if (key === 'maxTokens' && (!integer(value) || value < 1 || value > 4096)) {
                throw new RangeErrorType('EVAL_MODEL_INPUT: maxTokens must be an integer from 1 to 4096');
            }
            if (key === 'concurrency' && (!integer(value) || value < 1 || value > 8)) {
                throw new RangeErrorType('EVAL_MODEL_INPUT: concurrency must be an integer from 1 to 8');
            }
            args[allowed[key]] = value;
        }
        return args;
    };
    const launchModel = async (operation, args) => {
        const encoded = await callTool(modelOperations[operation],args,false);
        let started;
        try { started = parse(encoded); } catch (_) { throw modelProtocol(); }
        const timeoutSeconds = hasOwn(args,'timeout_secs') ? args.timeout_secs
            : operation === 'judgment_batch' ? 300 : 60;
        if (!modelRecord(started) || started.schema !== 'pi.eval.model-job.v1'
            || started.operation !== operation || !modelJobId(started.jobId)
            || started.timeoutSeconds !== timeoutSeconds) throw modelProtocol();
        // Invalid or lost acknowledgment is never retried. The native job may
        // have started; recover its id from jobs before explicitly attaching.
        return new ModelJob(started.jobId,operation);
    };
    const attachModel = async (operation, id) => {
        if (!modelJobId(id)) throw new TypeErrorType('EVAL_MODEL_INPUT: attach requires a bounded native job id');
        const job = new ModelJob(id,operation);
        await job.status();
        return job;
    };
    const completion = async (prompt, options = {}) => {
        if (typeof prompt !== 'string' || !prompt.trim()) throw new TypeErrorType('EVAL_MODEL_INPUT: completion requires a nonempty prompt');
        const args = modelOptions(options,{model:'model',system:'system',schema:'schema',images:'images',maxTokens:'max_tokens',timeoutSeconds:'timeout_secs'});
        return launchModel('completion',{prompt,...args});
    };
    const judge = async (state, questions, options = {}) => {
        if (!modelRecord(questions) || keys(questions).length === 0) throw new TypeErrorType('EVAL_MODEL_INPUT: questions must be a nonempty object');
        return launchModel('judgment',{state,questions,
            ...modelOptions(options,{model:'model',images:'images',timeoutSeconds:'timeout_secs'})});
    };
    const judgeBatch = async (states, questions, options = {}) => {
        if ((!isArray(states) && !modelRecord(states)) || keys(states).length === 0
            || !modelRecord(questions) || keys(questions).length === 0) {
            throw new TypeErrorType('EVAL_MODEL_INPUT: judgeBatch requires nonempty states and questions');
        }
        return launchModel('judgment_batch',{states,questions,
            ...modelOptions(options,{model:'model',images:'images',concurrency:'concurrency',timeoutSeconds:'timeout_secs'})});
    };
    completion.attach = id => attachModel('completion',id);
    judge.attach = id => attachModel('judgment',id);
    judgeBatch.attach = id => attachModel('judgment_batch',id);
    globalThis.completion = freeze(completion);
    globalThis.judge = freeze(judge);
    globalThis.judgeBatch = freeze(judgeBatch);
    const workerStates = new WeakMap();
    const workerGet = workerStates.get.bind(workerStates);
    const workerSet = workerStates.set.bind(workerStates);
    const workerControls = new WeakMap();
    const workerControlGet = workerControls.get.bind(workerControls);
    const workerControlSet = workerControls.set.bind(workerControls);
    const workerControl = worker => {
        const state = workerControlGet(worker);
        if (state.pending > 0 && state.generation !== cellGeneration) {
            // The previous cell may have submitted a turn but lost its reply.
            // Do not reinterpret that as an unsubmitted prompt or replay it.
            state.uncertain = true;
            state.allowSend = false;
        }
        return state;
    };
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
        constructor(snapshot, allowSend = true, recovered = false) {
            this.id = snapshot.workerId;
            this.workerId = snapshot.workerId;
            this.jobId = snapshot.jobId;
            this.agent = snapshot.agent;
            this.recovered = recovered;
            workerSet(this, snapshot.turnId);
            workerControlSet(this, {allowSend, uncertain:false, pending:0, generation:cellGeneration});
            freeze(this);
        }
        get turnId() { return workerGet(this); }
        get sendAllowed() { return workerControl(this).allowSend; }
        get unknownAdmission() { return workerControl(this).uncertain; }
        async turn(turnId) {
            if (!validTurnId(turnId)) throw new TypeErrorType('EVAL_AGENT_INPUT: turn requires an integer from 1 to 32');
            const turn = new AgentTurn(this, turnId);
            await readWorker(turn, 'worker_status');
            return turn;
        }
        async send(prompt) {
            if (typeof prompt !== 'string' || !prompt.trim()) {
                throw new TypeErrorType('EVAL_AGENT_INPUT: send requires a nonempty prompt');
            }
            const state = workerControl(this);
            if (!state.allowSend) {
                throw new ErrorType('EVAL_AGENT_RECOVERY_REQUIRED: sends are disabled on this handle; inspect native turns before explicitly attaching with allowSend:true');
            }
            const previous = workerGet(this);
            const generation = cellGeneration;
            state.pending++;
            state.generation = generation;
            try {
                const snapshot = checkWorker(await nativeResult('subagent', {
                    operation:'worker_send', workerId:this.id, task:prompt,
                }), this.id);
                if (generation !== cellGeneration || snapshot.jobId !== this.jobId
                    || snapshot.agent !== this.agent || snapshot.turnId <= previous) {
                    throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid worker send acknowledgment');
                }
                // Concurrent sends may resolve out of order. The newest native
                // turn stays selected; earlier returned handles remain fixed.
                workerSet(this, Math.max(workerGet(this), snapshot.turnId));
                return new AgentTurn(this, snapshot.turnId);
            } catch (error) {
                state.uncertain = true;
                state.allowSend = false;
                // Includes transport/protocol/host errors: the guest cannot
                // establish whether submission occurred. Preserve the cause.
                throw error;
            } finally {
                state.pending--;
            }
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
    const recoveryOptions = (id, options, flag, allowTurn = false) => {
        if (!modelJobId(id) || !modelRecord(options)
            || keys(options).some(key => key !== flag && !(allowTurn && key === 'turnId'))
            || (hasOwn(options,flag) && typeof options[flag] !== 'boolean')
            || (hasOwn(options,'turnId') && !validTurnId(options.turnId))) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: recovery requires an exact native id and supported typed options');
        }
    };
    launchAgent.attachWorker = async (id, options = {}) => {
        recoveryOptions(id, options, 'allowSend', true);
        const allowSend = hasOwn(options,'allowSend') && options.allowSend === true;
        const args = {operation:'worker_status',workerId:id};
        const turnId = hasOwn(options,'turnId') ? options.turnId : undefined;
        if (turnId !== undefined) args.turnId = turnId;
        const snapshot = checkWorker(await nativeResult('subagent',args),id,turnId);
        if (!modelJobId(snapshot.jobId)) throw new ErrorType('EVAL_AGENT_PROTOCOL: invalid native worker identity');
        return new AgentWorker(snapshot,allowSend,true);
    };
    globalThis.agent = launchAgent;
    globalThis.wait = async (handles, options = {}) => {
        if (!isArray(handles) || handles.length > 32 || handles.some(handle =>
            !(handle instanceof AgentJob) && !(handle instanceof AgentTurn)
            && !(handle instanceof AgentPoolItem) && !(handle instanceof ModelJob) && !workerGet(handle))
            || !options || typeof options !== 'object' || isArray(options)
            || keys(options).some(name => name !== 'timeout' && name !== 'raiseErrors')) {
            throw new TypeErrorType('EVAL_AGENT_INPUT: wait requires up to 32 agent/model handles and timeout/raiseErrors options');
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
                : handle instanceof ModelJob ? waitModelJob(handle, remaining, raiseErrors)
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
        if (hasOwn(snapshot,'workerContexts') && (!isArray(snapshot.workerContexts)
            || snapshot.workerContexts.length !== snapshot.concurrency
            || snapshot.workerContexts.some((worker,index) => !modelRecord(worker) || worker.workerIndex !== index + 1
                || (worker.tokens !== null && (!integer(worker.tokens) || worker.tokens < 0))
                || (worker.window !== null && (!integer(worker.window) || worker.window <= 0))))) {
            throw new ErrorType('EVAL_POOL_PROTOCOL: invalid native worker context snapshot');
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
    const poolDrainInput = () => new TypeErrorType('EVAL_POOL_INPUT: drain requires a pool cursor, timeout seconds from 0 through 600 and maxItems from 1 through 4');
    const poolDrainProtocol = () => new ErrorType('EVAL_POOL_PROTOCOL: invalid native pool drain response');
    const poolDrainCursor = (value, poolId, fail) => {
        if (value === null) return null;
        const cursor = drainRecord(value,['schema','poolId','itemIds'],fail);
        if (keys(cursor).length !== 3 || cursor.schema !== 'pi.subagent.pool.cursor.v1'
            || cursor.poolId !== poolId || !isArray(cursor.itemIds) || cursor.itemIds.length > 32) throw fail();
        const itemIds = [];
        for (let index = 0; index < cursor.itemIds.length; index++) {
            const field = drainDescriptor(cursor.itemIds,String(index));
            if (!field || !hasOwn(field,'value') || !integer(field.value) || field.value < 1 || field.value > 32
                || (index > 0 && field.value <= itemIds[index - 1])) throw fail();
            itemIds.push(field.value);
        }
        if (drainOwnKeys(cursor.itemIds).length !== itemIds.length + 1) throw fail();
        // The acknowledgment is copied before queueing or awaiting; inherited
        // serializers cannot change which native results it acknowledges.
        drainSetPrototype(itemIds,null);
        cursor.itemIds = itemIds;
        return cursor;
    };
    const checkPoolDrain = (page, state, previous, maximum) => {
        if (!modelRecord(page) || page.schema !== 'pi.subagent.pool.drain.v1'
            || page.poolId !== state.poolId || page.jobId !== state.jobId || page.agent !== state.options.agent
            || !validPoolStatus(page.status) || typeof page.sealed !== 'boolean'
            || (page.status === 'open') === page.sealed
            || !integer(page.admittedItems) || page.admittedItems < state.items.length || page.admittedItems > 32
            || !integer(page.settledItems) || page.settledItems < 0 || page.settledItems > page.admittedItems
            || page.pendingItems !== page.admittedItems - page.settledItems
            || (poolTerminal(page.status) && page.pendingItems !== 0)
            || !isArray(page.items) || page.items.length > maximum
            || typeof page.hasMore !== 'boolean' || typeof page.done !== 'boolean' || typeof page.timedOut !== 'boolean') {
            throw poolDrainProtocol();
        }
        const cursor = poolDrainCursor(page.cursor,state.poolId,poolDrainProtocol);
        const next = poolDrainCursor(page.nextCursor,state.poolId,poolDrainProtocol);
        if (cursor === null || next === null) throw poolDrainProtocol();
        const before = previous === null ? [] : previous.itemIds;
        if (cursor.itemIds.length !== before.length) throw poolDrainProtocol();
        for (let index = 0; index < before.length; index++) {
            if (cursor.itemIds[index] !== before[index]) throw poolDrainProtocol();
        }
        const added = [];
        let last = 0;
        for (const item of page.items) {
            if (!modelRecord(item) || !integer(item.id) || item.id <= last || item.id > page.admittedItems
                || drainHas(before,item.id) || !['completed','failed','cancelled'].includes(item.status)
                || !modelRecord(item.result) || !isArray(item.result.content) || !hasOwn(item.result,'details')
                || (hasOwn(item.result,'isError') && typeof item.result.isError !== 'boolean')) throw poolDrainProtocol();
            last = item.id;
            added.push(item.id);
        }
        if (next.itemIds.length !== before.length + added.length || next.itemIds.length > page.settledItems) throw poolDrainProtocol();
        for (let index = 0; index < next.itemIds.length; index++) {
            const id = next.itemIds[index];
            if (id > page.admittedItems || (!drainHas(before,id) && !drainHas(added,id))) throw poolDrainProtocol();
        }
        if (page.hasMore !== (page.settledItems > next.itemIds.length)
            || (page.hasMore && page.items.length !== maximum)
            || page.done !== (poolTerminal(page.status) && next.itemIds.length === page.admittedItems)
            || page.timedOut !== (page.items.length === 0 && !page.done)) throw poolDrainProtocol();
        return page;
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
            ({...item,resultRef:{...item.resultRef}})),
            ...(state.snapshot.workerContexts ? {workerContexts:state.snapshot.workerContexts.map(worker => ({...worker}))} : {}),
        } : {
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
                unknownAdmission:false,admissionPending:false,lastError:null,recovered:false,
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
        get recovered() { return poolGet(this).recovered; }
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
        async drain(options = {}) {
            const state = poolGet(this);
            const fields = drainRecord(options,['cursor','timeout','maxItems'],poolDrainInput);
            const seconds = hasOwn(fields,'timeout') ? fields.timeout : 0;
            const maximum = hasOwn(fields,'maxItems') ? fields.maxItems : 4;
            if (typeof seconds !== 'number' || !finite(seconds) || seconds < 0 || seconds > 600
                || !integer(maximum) || maximum < 1 || maximum > 4 || state.poolId === null) throw poolDrainInput();
            const cursor = poolDrainCursor(hasOwn(fields,'cursor') ? fields.cursor : null,state.poolId,poolDrainInput);
            return serialPool(state, async () => {
                const args = drainCreate(null);
                args.operation = 'pool_drain';
                args.poolId = state.poolId;
                if (cursor !== null) args.cursor = cursor;
                args.timeoutMs = Math.floor(seconds * 1000);
                args.maxItems = maximum;
                const page = checkPoolDrain(await nativeResult('subagent',args),state,cursor,maximum);
                state.closed ||= page.sealed;
                // Observation never acknowledges a page implicitly. A failed
                // read can be retried with the same cursor without replaying work.
                return page;
            });
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
    const workpool = async (options = {}) => {
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
    workpool.attach = async (id, options = {}) => {
        recoveryOptions(id, options, 'allowPush');
        const allowPush = hasOwn(options,'allowPush') && options.allowPush === true;
        const snapshot = checkPool(await nativeResult('subagent',{operation:'pool_status',poolId:id}),id);
        if (!modelJobId(snapshot.jobId) || !boundedText(snapshot.agent,128)) {
            throw new ErrorType('EVAL_POOL_PROTOCOL: invalid recovered pool identity');
        }
        // No pool_start, task replay, launch options or renewed timeout. A
        // known ID suffices because further pushes use the native frozen policy.
        const pool = new AgentPool(freeze({agent:snapshot.agent}),'recovered-workpool',snapshot.concurrency);
        const state = poolGet(pool);
        adoptPool(state,snapshot);
        state.recovered = true;
        state.closed ||= !allowPush;
        // Already-sealed pools can yield their aggregate without resending
        // pool_finish. Failed/cancelled aggregates retain ordinary wait errors.
        state.finishAcknowledged = snapshot.sealed;
        return pool;
    };
    globalThis.workpool = workpool;
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
