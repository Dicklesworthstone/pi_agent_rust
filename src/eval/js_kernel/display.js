// This factory captures its intrinsics before cells run. Only the native sink
// owns captured frames; guest values are snapshotted before entering that sink.
(nativeDisplay => {
    "use strict";
    const stringify = JSON.stringify;
    const keys = Reflect.ownKeys;
    const descriptor = Object.getOwnPropertyDescriptor;
    const own = Object.prototype.hasOwnProperty;
    const prototype = Object.getPrototypeOf;
    const plainPrototype = Object.prototype;
    const isArray = Array.isArray;
    const finite = Number.isFinite;
    const apply = Reflect.apply;
    const charCode = String.prototype.charCodeAt;
    const ErrorType = TypeError;
    const SetType = Set;
    const has = Set.prototype.has;
    const add = Set.prototype.add;
    const remove = Set.prototype.delete;
    const isView = ArrayBuffer.isView;
    const typedPrototype = prototype(Uint8Array.prototype);
    const typedLength = descriptor(typedPrototype, "length").get;
    const typedName = descriptor(typedPrototype, Symbol.toStringTag).get;
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const maxBytes = 2 * 1024 * 1024;

    const fail = message => { throw new ErrorType(message); };
    const invalid = () => fail("EVAL_DISPLAY_INVALID: explicit JSON data, text or PNG bytes/base64 required");
    const limit = () => fail("EVAL_DISPLAY_LIMIT: rich output exceeds its byte or structure budget");
    const codeAt = (text, index) => apply(charCode, text, [index]);

    function pngData(value) {
        if (typeof value === "string") return value;
        if (!isView(value) || apply(typedName, value, []) !== "Uint8Array") return invalid();
        const length = apply(typedLength, value, []);
        if (length > 3 * (maxBytes / 4)) return limit();
        let encoded = "";
        for (let i = 0; i < length; i += 3) {
            const a = value[i], b = i + 1 < length ? value[i + 1] : 0;
            const c = i + 2 < length ? value[i + 2] : 0;
            encoded += alphabet[a >> 2] + alphabet[((a & 3) << 4) | (b >> 4)]
                + (i + 1 < length ? alphabet[((b & 15) << 2) | (c >> 6)] : "=")
                + (i + 2 < length ? alphabet[c & 63] : "=");
        }
        return encoded;
    }

    return function display(value, mimeType = "application/json") {
        if (mimeType !== "application/json" && mimeType !== "text/plain"
            && mimeType !== "text/markdown" && mimeType !== "image/png") return invalid();
        if (mimeType === "image/png") value = pngData(value);
        if (mimeType !== "application/json" && typeof value !== "string") return invalid();
        let output = "", bytes = 0, nodes = 0;
        const ancestors = new SetType();
        function emit(text) {
            // Text is already JSON encoded. Count UTF-8, not JS code units.
            for (let i = 0; i < text.length; i++) {
                const c = codeAt(text, i);
                if (c < 128) bytes++;
                else if (c < 2048) bytes += 2;
                else if (c >= 0xd800 && c <= 0xdbff) { bytes += 4; i++; }
                else bytes += 3;
                if (bytes > maxBytes) return limit();
            }
            output += text;
        }
        function quote(text) {
            if (text.length > maxBytes) return limit();
            // Rust JSON strings cannot represent unpaired UTF-16 surrogates.
            // Refuse rather than silently substituting different text.
            for (let i = 0; i < text.length; i++) {
                const c = codeAt(text, i);
                if (c >= 0xd800 && c <= 0xdbff) {
                    const next = codeAt(text, ++i);
                    if (!(next >= 0xdc00 && next <= 0xdfff)) return invalid();
                } else if (c >= 0xdc00 && c <= 0xdfff) return invalid();
            }
            emit(stringify(text));
        }
        function visit(data, depth) {
            if (++nodes > 65536 || depth > 32) return limit();
            if (data === null) return emit("null");
            switch (typeof data) {
                case "string": return quote(data);
                case "boolean": return emit(data ? "true" : "false");
                case "number":
                    if (!finite(data)) return invalid();
                    return emit(stringify(data));
                case "object": break;
                default: return invalid();
            }
            const array = isArray(data);
            if (!array && prototype(data) !== plainPrototype && prototype(data) !== null) return invalid();
            if (apply(has, ancestors, [data])) return fail("EVAL_DISPLAY_INVALID: cyclic JSON data");
            apply(add, ancestors, [data]);
            const names = keys(data);
            if (names.length > 65537) return limit();
            if (array) {
                const length = descriptor(data, "length").value;
                if (length > 65535) return limit();
                // Do not silently discard custom array properties or holes.
                if (names.length !== length + 1) return invalid();
                emit("[");
                for (let i = 0; i < length; i++) {
                    const entry = descriptor(data, "" + i);
                    if (!entry || !apply(own, entry, ["value"])) return invalid();
                    if (i) emit(",");
                    visit(entry.value, depth + 1);
                }
                emit("]");
            } else {
                emit("{");
                let first = true;
                for (let i = 0; i < names.length; i++) {
                    const name = names[i], entry = descriptor(data, name);
                    if (!entry || !entry.enumerable) continue;
                    if (typeof name !== "string" || !apply(own, entry, ["value"])) return invalid();
                    if (!first) emit(",");
                    first = false;
                    quote(name); emit(":"); visit(entry.value, depth + 1);
                }
                emit("}");
            }
            apply(remove, ancestors, [data]);
        }
        emit('{"mimeType":'); quote(mimeType); emit(',"data":');
        visit(value, 0); emit("}");
        // No reference to value, callback, or guest-owned object is retained.
        nativeDisplay(output);
    };
})
