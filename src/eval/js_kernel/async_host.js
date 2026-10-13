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
    globalThis.tool.call = (name, args = {}) => new PromiseType((resolve, reject) => {
        if (typeof name !== 'string' || args === null || typeof args !== 'object' || isArray(args)) {
            throw new TypeErrorType('tool.call requires a tool name and object arguments');
        }
        const id = enqueue(name, stringify(args));
        callSet(id, {resolve, reject});
    });
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
                    if (flag) call.resolve(content);
                    else call.reject(new ErrorType(content));
                }
            }
        },
        clear() {
            timerClear();
            callClear();
        },
    };
})(__pi_set_timer, __pi_clear_timer, __pi_async_call);
