// A realm-owned CommonJS cache, not a Node host or another interpreter.
// The native callback admits every edge under the CURRENT cell, including
// cache hits. Its records contain only already-admitted workspace source.
((host, slot) => {
    const apply = Reflect.apply;
    const define = Object.defineProperty;
    const parse = JSON.parse;
    const compile = Function;
    const TypeErrorType = TypeError;
    const ErrorType = Error;
    const get = Map.prototype.get;
    const set = Map.prototype.set;
    const has = Map.prototype.has;
    const remove = Map.prototype.delete;
    const charCodeAt = String.prototype.charCodeAt;
    const indexOf = String.prototype.indexOf;
    const slice = String.prototype.slice;
    const cache = new Map();
    let depth = 0;

    function resolve(base, specifier) {
        if (typeof specifier !== 'string' || specifier.length === 0) {
            throw new TypeErrorType('EVAL_COMMONJS_SPECIFIER: require expects a nonempty string');
        }
        return host('resolve', base, specifier);
    }

    function load(identity) {
        // Loading an old module's dependency never retains the old cell's
        // permissions. Even an already cached value needs a live owner.
        host('check', identity, '');
        if (apply(has, cache, [identity])) {
            return apply(get, cache, [identity]).exports;
        }
        if (depth >= 64) {
            throw new ErrorType('EVAL_COMMONJS_DEPTH: require nesting exceeds 64 modules');
        }
        depth++;
        let inserted = false;
        try {
            const data = parse(host('read', identity, ''));
            const filename = data.path;
            const dirname = data.directory;
            const module = {id: filename, filename, loaded: false, exports: {}};
            // Publish the partial exports BEFORE running code, so dependency
            // cycles see the same object rather than recursively re-executing.
            apply(set, cache, [identity, module]);
            inserted = true;
            let source = data.source;
            if (apply(charCodeAt, source, [0]) === 0xfeff) {
                source = apply(slice, source, [1]);
            }
            if (data.kind === 'json') {
                try {
                    module.exports = parse(source);
                } catch (_) {
                    // Data parser diagnostics must not quote private file bytes.
                    throw new ErrorType('EVAL_MODULE_JSON: file is not valid JSON');
                }
            } else if (data.kind === 'commonjs') {
                const require = specifier => load(resolve(identity, specifier));
                define(require, 'resolve', {value: specifier =>
                    host('path', resolve(identity, specifier), '')});
                module.require = require;
                if (apply(slice, source, [0, 2]) === '#!') {
                    const newline = apply(indexOf, source, ['\n']);
                    source = newline < 0 ? '' : apply(slice, source, [newline]);
                }
                // The wrapper owns its local variables and top-level return.
                // It receives neither host callback nor cache through scope.
                const body = compile('exports', 'require', 'module', '__filename', '__dirname', source);
                apply(body, module.exports, [module.exports, require, module, filename, dirname]);
            } else {
                throw new ErrorType('EVAL_COMMONJS_FORMAT: unsupported module format');
            }
            host('check', identity, '');
            module.loaded = true;
            return module.exports;
        } catch (error) {
            // Never replay automatically. An explicit later require may retry
            // a failed module; native failed reads/compiles still consume budget.
            if (inserted) apply(remove, cache, [identity]);
            throw error;
        } finally {
            depth--;
        }
    }

    // Native module metadata receives this immutable entrypoint. All retained
    // JS values are traced by the realm, not Rust Persistent roots/cycles.
    define(globalThis, slot, {value: load, writable: false, configurable: false});
    return load;
})
