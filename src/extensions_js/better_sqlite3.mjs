// The synchronous better-sqlite3 operations used by imported extensions.
// Every database operation goes through bun:sqlite's existing native owner,
// storage, SQL, cancellation and resource checks. This module grants no I/O.
import { Database as NativeDatabase } from "bun:sqlite";
import { Buffer } from "node:buffer";

const databases = new WeakMap();
const statementKey = Symbol("PiJS better-sqlite3 statement");

function state(database) {
  const value = databases.get(database);
  if (!value) throw new TypeError("Invalid better-sqlite3 database receiver");
  return value;
}

function openState(database) {
  const value = state(database);
  if (!value.open) throw new TypeError("The database connection is not open");
  return value;
}

function boolean(value, name) {
  if (typeof value !== "boolean") throw new TypeError(`${name} must be a boolean`);
  return value;
}

function binding(value) {
  if (value === null || typeof value === "string" || typeof value === "bigint"
      || (typeof value === "number" && Number.isFinite(value))
      || value instanceof ArrayBuffer || ArrayBuffer.isView(value)) return value;
  throw new TypeError("SQLite bindings must be finite numbers, strings, bigints, null or byte buffers");
}

function parameters(args) {
  const values = args.flat(1);
  const named = values.filter(value => value !== null && typeof value === "object"
    && !(value instanceof ArrayBuffer) && !ArrayBuffer.isView(value));
  if (named.length) {
    if (values.length !== 1 || Array.isArray(named[0])) {
      throw new TypeError("PiJS SQLite accepts either positional bindings or one named-binding object, not a mixture");
    }
    const prototype = Object.getPrototypeOf(named[0]);
    if (prototype !== Object.prototype && prototype !== null) {
      throw new TypeError("Named SQLite bindings must be a plain object");
    }
    return [Object.fromEntries(Object.entries(named[0]).map(([key, value]) => [key, binding(value)]))];
  }
  return values.map(binding);
}

function output(value, safeIntegers) {
  if (typeof value === "bigint") return safeIntegers ? value : Number(value);
  if (value instanceof Uint8Array) return Buffer.from(value);
  return value;
}

function rowOutput(row, safeIntegers) {
  // fromEntries preserves column names such as __proto__ as ordinary data.
  return Object.fromEntries(Object.entries(row)
    .map(([name, value]) => [name, output(value, safeIntegers)]));
}

export class Statement {
  #database;
  #native;
  #source;
  #bound = false;
  #safeIntegers;

  constructor(key, database, native, source) {
    if (key !== statementKey) throw new TypeError("Use Database.prepare() to create a Statement");
    this.#database = database;
    this.#native = native;
    this.#source = source;
    this.#safeIntegers = openState(database).safeIntegers;
  }

  #arguments(args) {
    openState(this.#database);
    if (this.#bound) {
      if (args.length) throw new TypeError("This statement already has bound parameters");
      return [];
    }
    const values = parameters(args);
    const named = values.length === 1 && values[0] !== null && typeof values[0] === "object"
      && !(values[0] instanceof ArrayBuffer) && !ArrayBuffer.isView(values[0]);
    if (!named && values.length !== this.#native.paramsCount) {
      throw new RangeError(`SQLite expected ${this.#native.paramsCount} parameters, received ${values.length}`);
    }
    return values;
  }

  #requireReader() {
    openState(this.#database);
    if (!this.reader) throw new TypeError("This statement does not return data; use run()");
  }

  run(...args) {
    const result = this.#native.run(...this.#arguments(args));
    return { changes: result.changes, lastInsertRowid: output(result.lastInsertRowid, this.#safeIntegers) };
  }

  get(...args) {
    this.#requireReader();
    const row = this.#native.get(...this.#arguments(args));
    return row === null ? undefined : rowOutput(row, this.#safeIntegers);
  }

  all(...args) {
    this.#requireReader();
    return this.#native.all(...this.#arguments(args)).map(row => rowOutput(row, this.#safeIntegers));
  }

  iterate(...args) {
    // Like the native PiJS iterator, this materializes a bounded result before
    // exposing rows. It never leaves an engine cursor or transaction detached.
    return this.all(...args)[Symbol.iterator]();
  }

  bind(...args) {
    if (this.#bound) throw new TypeError("This statement already has bound parameters");
    this.#native.bind(...this.#arguments(args));
    this.#bound = true;
    return this;
  }

  safeIntegers(enabled = true) {
    openState(this.#database);
    this.#safeIntegers = boolean(enabled, "safeIntegers");
    return this;
  }

  get database() { return this.#database; }
  get source() { return this.#source; }
  get reader() { return this.#native.columnNames.length > 0; }
  get busy() { return false; }
}

class Connection {
  constructor(filename = "", options = {}) {
    if (typeof filename !== "string") throw new TypeError("SQLite filename must be a string");
    if (!options || typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError("SQLite options must be an object");
    }
    for (const key of Object.keys(options)) {
      if (!["readonly", "fileMustExist", "timeout"].includes(key)) {
        throw new TypeError(`Unsupported better-sqlite3 option in PiJS: ${key}`);
      }
    }
    const readonly = options.readonly === undefined ? false : boolean(options.readonly, "readonly");
    const mustExist = options.fileMustExist === undefined ? false
      : boolean(options.fileMustExist, "fileMustExist");
    const timeout = options.timeout === undefined ? 5000 : options.timeout;
    if (!Number.isInteger(timeout) || timeout < 0 || timeout > 30000) {
      throw new RangeError("PiJS SQLite timeout must be an integer from 0 to 30000 milliseconds");
    }
    const memory = filename === "" || filename === ":memory:";
    const native = new NativeDatabase(filename || ":memory:", {
      readonly, create: memory || (!readonly && !mustExist), strict: true, safeIntegers: true,
    });
    try {
      native.exec(`PRAGMA busy_timeout = ${timeout}`);
    } catch (error) {
      try { native.close(); } catch {}
      throw error;
    }
    databases.set(this, { native, open: true, name: filename, memory, readonly, safeIntegers: false });
  }

  prepare(sql) {
    const database = openState(this);
    return new Statement(statementKey, this, database.native.prepare(sql), sql);
  }

  exec(sql) {
    openState(this).native.exec(sql);
    return this;
  }

  pragma(source, options = {}) {
    const database = openState(this);
    if (typeof source !== "string" || !source.trim()) throw new TypeError("PRAGMA must be a nonempty string");
    if (!options || typeof options !== "object" || Array.isArray(options)) {
      throw new TypeError("PRAGMA options must be an object");
    }
    for (const key of Object.keys(options)) {
      if (key !== "simple") throw new TypeError(`Unsupported PRAGMA option: ${key}`);
    }
    const simple = options.simple === undefined ? false : boolean(options.simple, "simple");
    const statement = database.native.prepare(`PRAGMA ${source}`);
    const rows = statement.values();
    if (simple) return rows.length ? output(rows[0][0], database.safeIntegers) : undefined;
    const columns = statement.columnNames;
    return rows.map(row => Object.fromEntries(columns.map((name, index) =>
      [name, output(row[index], database.safeIntegers)])));
  }

  transaction(fn) {
    const transaction = openState(this).native.transaction(fn);
    const variants = {
      deferred: transaction.deferred, immediate: transaction.immediate, exclusive: transaction.exclusive,
    };
    for (const wrapper of new Set(Object.values(variants))) {
      Object.assign(wrapper, variants);
      Object.defineProperty(wrapper, "database", { value: this });
    }
    return transaction;
  }

  defaultSafeIntegers(enabled = true) {
    openState(this).safeIntegers = boolean(enabled, "defaultSafeIntegers");
    return this;
  }

  close() {
    const database = state(this);
    if (database.open) {
      database.native.close();
      database.open = false;
    }
    return this;
  }

  loadExtension() { return openState(this).native.loadExtension(); }
  get open() { return state(this).open; }
  get inTransaction() { const database = state(this); return database.open && database.native.inTransaction; }
  get name() { return state(this).name; }
  get memory() { return state(this).memory; }
  get readonly() { return state(this).readonly; }
}

// The CommonJS API is callable with or without `new`; both import forms share
// the same constructor and native connection implementation.
export function Database(filename, options) { return new Connection(filename, options); }
Database.prototype = Connection.prototype;
Object.defineProperty(Connection.prototype, "constructor", { value: Database, configurable: true, writable: true });
Database.Database = Database;
Database.Statement = Statement;
if (typeof Symbol.dispose === "symbol") {
  Object.defineProperty(Connection.prototype, Symbol.dispose, { value() { this.close(); } });
}
export default Database;
