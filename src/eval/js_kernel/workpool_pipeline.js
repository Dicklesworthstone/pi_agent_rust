// Bounded streaming orchestration over the existing session-owned native pool.
// This bootstrap retains no native authority and starts no independent task.
((createPool) => {
    const ErrorType = Error;
    const TypeErrorType = TypeError;
    const integer = Number.isInteger;
    const finite = Number.isFinite;
    const isArray = Array.isArray;
    const ownKeys = Reflect.ownKeys;
    const descriptor = Object.getOwnPropertyDescriptor;
    const freeze = Object.freeze;
    const define = Object.defineProperty;
    const create = Object.create;
    const hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    const clock = Date.now;
    const MapType = Map;
    const iteratorKey = Symbol.iterator;
    const asyncIteratorKey = Symbol.asyncIterator;
    const inputError = () => new TypeErrorType('EVAL_PIPELINE_INPUT: expected an iterable, map/reduce callbacks and supported typed options');

    // Do not run option getters or inherited serializers while validating the
    // orchestration policy. Native pool options keep their existing validator.
    const optionsRecord = options => {
        if (options === null || typeof options !== 'object' || isArray(options)) throw inputError();
        const fields = create(null);
        for (const key of ownKeys(options)) {
            if (!['pool', 'maxItems', 'timeout', 'raiseErrors', 'onPool', 'ordered'].includes(key)) throw inputError();
            const field = descriptor(options, key);
            if (!field || !hasOwn(field, 'value')) throw inputError();
            fields[key] = field.value;
        }
        return fields;
    };
    const protocolError = () => new ErrorType('EVAL_PIPELINE_PROTOCOL: inconsistent native pool result');
    const timeoutError = () => {
        const error = new ErrorType('EVAL_PIPELINE_TIMEOUT: shared pipeline wait budget exhausted');
        error.name = 'TimeoutError';
        return error;
    };
    const cursorAfter = (cursor, poolId, itemId) => {
        const ids = cursor === null ? [] : [...cursor.itemIds];
        if (ids.includes(itemId)) throw protocolError();
        ids.push(itemId);
        ids.sort((left, right) => left - right);
        return freeze({schema: 'pi.subagent.pool.cursor.v1', poolId, itemIds: freeze(ids)});
    };

    async function mapReduce(source, mapTask, reduce, initial, options = {}) {
        const fields = optionsRecord(options);
        const maxItems = hasOwn(fields, 'maxItems') ? fields.maxItems : 32;
        const timeout = hasOwn(fields, 'timeout') ? fields.timeout : 30;
        const raiseErrors = hasOwn(fields, 'raiseErrors') ? fields.raiseErrors : true;
        const ordered = hasOwn(fields, 'ordered') ? fields.ordered : false;
        const onPool = fields.onPool;
        const poolOptions = hasOwn(fields, 'pool') ? fields.pool : {};
        if (typeof mapTask !== 'function' || typeof reduce !== 'function'
            || !integer(maxItems) || maxItems < 1 || maxItems > 32
            || typeof timeout !== 'number' || !finite(timeout) || timeout <= 0 || timeout > 600
            || typeof raiseErrors !== 'boolean' || typeof ordered !== 'boolean'
            || (onPool !== undefined && typeof onPool !== 'function')
            || source === null || source === undefined || typeof source === 'string'
            || (isArray(source) && source.length > maxItems)) throw inputError();
        const makeIterator = source[asyncIteratorKey] ?? source[iteratorKey];
        if (typeof makeIterator !== 'function') throw inputError();
        const iterator = makeIterator.call(source);
        if (!iterator || typeof iterator.next !== 'function') throw inputError();
        const next = iterator.next.bind(iterator);
        const deadline = clock() + timeout * 1000;
        const pending = new MapType();
        const buffered = new MapType();
        let pool = null;
        let cursor = null;
        // Private receive position: buffered results must not be fetched
        // repeatedly while waiting for an earlier source item. Only `cursor`
        // acknowledges successful reductions and is exposed for recovery.
        let received = null;
        let value = initial;
        let admitted = 0;
        let completed = 0;
        let succeeded = 0;
        let failed = 0;
        let exhausted = false;
        let sealed = false;
        let stage = 'create';
        let current = null;
        const remaining = () => {
            const seconds = (deadline - clock()) / 1000;
            if (seconds <= 0) throw timeoutError();
            return Math.min(seconds, 600);
        };
        const progress = () => freeze({value, admitted, completed, succeeded, failed});
        try {
            pool = await createPool(poolOptions);
            remaining();
            // The native pool owns its worker count, deadline, tools and model.
            const window = pool.concurrency;
            if (!integer(window) || window < 1 || window > 8) throw protocolError();
            if (onPool !== undefined) {
                stage = 'onPool';
                await onPool(pool);
                remaining();
            }
            const fill = async () => {
                while (!exhausted && pending.size < window) {
                    stage = 'source';
                    current = null;
                    remaining();
                    const step = await next();
                    remaining();
                    if (!step || typeof step !== 'object') throw inputError();
                    if (step.done) {
                        exhausted = true;
                        break;
                    }
                    // One lookahead distinguishes exact capacity from overflow.
                    // Never silently truncate a generator or map the extra item.
                    if (admitted === maxItems) {
                        throw new ErrorType('EVAL_PIPELINE_CAPACITY: source exceeds maxItems; accepted work was not replayed');
                    }
                    const index = admitted;
                    const input = step.value;
                    current = freeze({index, input});
                    stage = 'map';
                    const prompt = await mapTask(input, index);
                    remaining();
                    if (typeof prompt !== 'string' || !prompt.trim()) throw inputError();
                    stage = 'admission';
                    const handles = await pool.push([prompt]);
                    // An absent/invalid acknowledgment is never retried, even
                    // if the host may already have accepted the prompt.
                    if (!isArray(handles) || handles.length !== 1
                        || handles[0].itemId !== index + 1 || handles[0].poolId !== pool.id) throw protocolError();
                    pending.set(handles[0].itemId, {index, input});
                    admitted++;
                    remaining();
                }
                if (exhausted && admitted > 0 && !sealed) {
                    stage = 'finish';
                    current = null;
                    await pool.finish();
                    sealed = true;
                    remaining();
                }
            };
            const reduceOne = async (id, outcome) => {
                const {item, ok} = outcome;
                current = item;
                stage = 'result';
                remaining();
                if (raiseErrors && !ok) {
                    throw new ErrorType('EVAL_PIPELINE_ITEM_FAILED: a native pool item did not succeed');
                }
                stage = 'reduce';
                value = await reduce(value, current);
                // Receiving is not acknowledgment. Retain an exact successful
                // reduction cursor even if another buffered reducer fails.
                cursor = cursorAfter(cursor, pool.id, id);
                pending.delete(id);
                buffered.delete(id);
                completed++;
                if (ok) succeeded++; else failed++;
                remaining();
            };
            await fill();
            // workpool() is lazy: empty input creates no native pool or worker.
            if (admitted === 0) return freeze({...progress(), pool, cursor, status: 'empty'});
            for (;;) {
                stage = 'drain';
                current = null;
                const page = await pool.drain({cursor: received, timeout: remaining(), maxItems: 4});
                if (!page || !isArray(page.items) || typeof page.done !== 'boolean'
                    || page.poolId !== pool.id) throw protocolError();
                for (const item of page.items) {
                    const record = pending.get(item.id);
                    if (!record || !['completed', 'failed', 'cancelled'].includes(item.status)
                        || !item.result || !isArray(item.result.content)
                        || !hasOwn(item.result, 'details')) throw protocolError();
                    const id = item.id;
                    const ok = item.status === 'completed' && !item.result.isError;
                    const outcome = {ok, item: freeze({...item, index: record.index, input: record.input})};
                    received = cursorAfter(received, pool.id, id);
                    if (ordered) {
                        buffered.set(id, outcome);
                        // Pending includes buffered results: the original
                        // admission window bounds both input and reordering.
                        while (buffered.has(completed + 1)) {
                            const nextId = completed + 1;
                            await reduceOne(nextId, buffered.get(nextId));
                        }
                    } else {
                        await reduceOne(id, outcome);
                    }
                }
                if (page.done) {
                    if (!exhausted || !sealed || pending.size !== 0) throw protocolError();
                    if (raiseErrors && page.status !== 'completed') {
                        stage = 'finish';
                        throw new ErrorType('EVAL_PIPELINE_POOL_FAILED: native pool terminated unsuccessfully');
                    }
                    return freeze({...progress(), pool, cursor, status: page.status});
                }
                if (page.items.length === 0) throw timeoutError();
                await fill();
            }
        } catch (cause) {
            const error = new ErrorType('EVAL_PIPELINE_FAILED: pipeline stopped; inspect cause, pool, cursor and progress before recovery');
            error.name = 'WorkpoolPipelineError';
            error.cause = cause;
            error.stage = stage;
            error.pool = pool;
            error.cursor = cursor;
            error.progress = progress();
            error.item = current;
            error.pending = freeze([...pending].map(([itemId, record]) =>
                freeze({itemId, index: record.index, input: record.input})));
            error.cleanupErrors = [];
            // Stop accepted work before invoking potentially arbitrary iterator
            // cleanup. Native close owns cancellation/reaping; no retry, new
            // carrier or replacement lifetime is introduced by this helper.
            if (pool !== null) {
                try { error.cleanup = await pool.close(); }
                catch (cleanupError) { error.cleanupErrors.push(cleanupError); }
            }
            if (!exhausted) {
                try {
                    const close = iterator.return;
                    if (typeof close === 'function') await close.call(iterator);
                } catch (cleanupError) { error.cleanupErrors.push(cleanupError); }
            }
            throw error;
        }
    }

    // Capture the original native facade at bootstrap, not a guest replacement
    // left in the global namespace by a previous cell.
    define(createPool, 'mapReduce', {value: mapReduce, enumerable: true});
})(globalThis.workpool);
