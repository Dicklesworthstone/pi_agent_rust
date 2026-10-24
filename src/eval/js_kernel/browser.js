// Scriptable browser handles over the existing invocation-owned tool bridge.
// No browser socket, session token, permission grant or retry lives in this realm.
((nativeResult) => {
    const freeze = Object.freeze;
    const keys = Object.keys;
    const own = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
    const descriptor = Object.getOwnPropertyDescriptor;
    const prototype = Object.getPrototypeOf;
    const objectPrototype = Object.prototype;
    const isArray = Array.isArray;
    const stringify = JSON.stringify;
    const create = Object.create;
    const define = Object.defineProperty;
    const symbols = Object.getOwnPropertySymbols;
    const finite = Number.isFinite;
    const ErrorType = Error;
    const TypeErrorType = TypeError;
    const references = new WeakMap();
    const getReference = references.get.bind(references);
    const setReference = references.set.bind(references);
    const invalid = message => new TypeErrorType(`EVAL_BROWSER_INPUT: ${message}`);
    const object = value => value !== null && typeof value === 'object' && !isArray(value)
        && (prototype(value) === objectPrototype || prototype(value) === null);
    const text = (value, name) => {
        if (typeof value !== 'string' || !value.trim() || value.length > 65536 || value.includes('\0')) {
            throw invalid(`${name} must be nonempty text of at most 65536 code units without NUL`);
        }
        return value;
    };
    // Reject values JSON would silently drop/coerce, before any native action.
    // Read data descriptors, not getters or toJSON hooks. The host remains the
    // authority for action schemas and its encoded request/response limits.
    const snapshot = value => {
        let left = 16384;
        let characters = 0;
        const visit = (item, depth) => {
            if (--left < 0 || depth > 32) throw invalid('arguments exceed depth/node limits');
            if (item === null || typeof item === 'boolean') return item;
            if (typeof item === 'number' && finite(item)) return item;
            if (typeof item === 'string') {
                characters += item.length;
                if (characters > 262144) throw invalid('arguments exceed the text budget');
                return item;
            }
            if (!isArray(item) && !object(item)) throw invalid('arguments must contain only JSON data');
            if (symbols(item).length) throw invalid('symbol argument keys are not supported');
            const names = keys(item);
            if (isArray(item) && (names.length !== item.length || names.some((key, index) => key !== `${index}`))) {
                throw invalid('argument arrays must be dense without extra properties');
            }
            const copied = isArray(item) ? [] : create(null);
            if (isArray(copied)) define(copied, 'toJSON', {value:undefined});
            for (const key of names) {
                characters += key.length;
                if (characters > 262144) throw invalid('arguments exceed the text budget');
                const field = descriptor(item, key);
                if (!field || !own(field, 'value')) throw invalid('argument getters are not supported');
                copied[key] = visit(field.value, depth + 1);
            }
            return copied;
        };
        return visit(value, 0);
    };
    const options = (value, reserved) => {
        if (!object(value)) throw invalid('options must be an object');
        const copied = snapshot(value);
        if (reserved.some(key => own(copied, key))) throw invalid('options cannot replace explicit action, tab, frame or input arguments');
        return copied;
    };
    const invoke = async args => {
        const result = await nativeResult('browser', snapshot(args));
        if (!result || result.schema !== 'pi.eval.tool_result.v1' || !isArray(result.content)
            || !own(result, 'details') || typeof result.isError !== 'boolean') {
            throw new ErrorType('EVAL_BROWSER_PROTOCOL: invalid native browser result');
        }
        if (result.isError) {
            const summary = result.content.filter(block => block && block.type === 'text' && typeof block.text === 'string')
                .map(block => block.text).join('\n').slice(0, 4096);
            const error = new ErrorType(`EVAL_BROWSER_ACTION: ${args.action} failed. ${summary}`);
            error.name = 'BrowserActionError';
            error.action = args.action;
            error.result = result;
            throw error;
        }
        return result;
    };
    const scoped = (name, frame, action, args = {}, fixed = {}) => {
        text(action, 'action');
        const copied = options(args, ['action', 'tab', 'frame', ...keys(fixed)]);
        return { ...copied, ...fixed, action, tab:name, ...(frame === null ? {} : {frame}) };
    };
    const locator = (name, frame, selector) => {
        text(selector, 'selector');
        const call = (action, args = {}, fixed = {}) => invoke(scoped(name, frame, action, args, {selector, ...fixed}));
        const handle = {
            selector,
            click: (args = {}) => call('click', args),
            hover: (args = {}) => call('hover', args),
            fill: (value, args = {}) => {
                if (typeof value !== 'string') throw invalid('fill requires literal string text');
                return call('fill', args, {text:value});
            },
            type: (value, args = {}) => {
                if (typeof value !== 'string') throw invalid('type requires literal string text');
                return call('type', args, {text:value});
            },
            press: (key, args = {}) => call('press', args, {key:text(key, 'key')}),
            waitFor: (args = {}) => call('wait_for', args),
            upload: (files, args = {}) => {
                if (!isArray(files) || !files.length || files.some(file => typeof file !== 'string')) {
                    throw invalid('upload requires an array of file paths');
                }
                return call('upload', args, {files});
            },
            download: (path, args = {}) => call('download', args, {output_path:text(path, 'output_path')}),
            dragTo: (target, args = {}) => {
                const reference = getReference(target);
                if (!reference || reference.name !== name || reference.frame !== frame) {
                    throw invalid('dragTo requires a locator in this same tab and frame');
                }
                return call('drag', args, {target_selector:reference.selector});
            },
        };
        setReference(handle, {name, frame, selector});
        return freeze(handle);
    };
    const tab = (name, frame = null, opened = null) => {
        text(name, 'tab name');
        if (frame !== null) text(frame, 'frame ID');
        const call = (action, args = {}) => invoke(scoped(name, frame, action, args));
        const select = selector => locator(name, frame, selector);
        return freeze({
            name, frameId:frame, opened,
            call,
            frame: id => tab(name, text(id, 'frame ID')),
            locator: select,
            ref: select,
            getByText: value => select(`text/${text(value, 'text')}`),
            getByLabel: value => select(`label/${text(value, 'label')}`),
            getByTestId: value => select(`testid/${text(value, 'test ID')}`),
            getByRole: (role, name, exact = true) => {
                if (typeof role !== 'string' || !/^[a-z][a-z0-9-]*$/.test(role) || typeof exact !== 'boolean') {
                    throw invalid('getByRole requires an accessible role and a boolean exact flag');
                }
                return select(`role/${role}[name=${stringify(text(name, 'accessible name'))}${exact ? ' exact' : ''}]`);
            },
            goto: (url, args = {}) => invoke(scoped(name, frame, 'goto', args, {url:text(url, 'URL')})),
            snapshot: (args = {}) => call('snapshot', args),
            accessibility: (args = {}) => call('ax_tree', args),
            frames: (args = {}) => call('list_frames', args),
            screenshot: (args = {}) => call('screenshot', args),
            scroll: (args = {}) => call('scroll', args),
            press: (key, args = {}) => invoke(scoped(name, frame, 'press', args, {key:text(key, 'key')})),
            back: (args = {}) => call('back', args),
            forward: (args = {}) => call('forward', args),
            reload: (args = {}) => call('reload', args),
            close: (args = {}) => call('close', args),
            evaluate: (script, args = {}) => invoke(scoped(name, frame, 'evaluate', args, {script:text(script, 'script')})),
            batch: (steps, args = {}) => {
                if (!isArray(steps) || !steps.length || steps.length > 16) throw invalid('batch requires 1-16 steps');
                const prepared = snapshot(steps).map(step => {
                    if (!object(step) || !object(step.call) || keys(step).some(key => !['call','bind','expect'].includes(key))) {
                        throw invalid('each batch step requires call and optional bind/expect');
                    }
                    const {action, ...fields} = step.call;
                    return {...step, call:scoped(name, frame, action, fields)};
                });
                const copied = options(args, ['action','tab','frame','steps']);
                if (keys(copied).some(key => key !== 'timeout_ms')) throw invalid('batch accepts only timeout_ms');
                return invoke({...copied, action:'batch', steps:prepared});
            },
        });
    };
    globalThis.browser = freeze({
        tab: name => tab(name),
        open: async (input) => {
            if (!object(input)) throw invalid('open requires {name, url, ...options}');
            const {name, url, ...args} = snapshot(input);
            text(name, 'tab name');
            text(url, 'URL');
            const opened = await invoke(scoped(name, null, 'open', args, {url}));
            return tab(name, null, opened);
        },
        tabs: (args = {}) => invoke({...options(args, ['action','tab','frame']), action:'list_tabs'}),
        status: () => invoke({action:'status'}),
        start: () => invoke({action:'start'}),
        stop: () => invoke({action:'stop'}),
    });
})(globalThis.tool.result);
