"""Streaming map/reduce using the kernel's existing native workpool facade.

No independent scheduler, child launch protocol, credential or tool authority is
introduced. The native pool remains responsible for admission and cancellation.
"""

import asyncio
import inspect
import json
import math
import time


def install(kernel):
    """Attach before cells run; capture the original, policy-checked factory."""
    create_pool = kernel._workpool
    clock = time.monotonic
    is_awaitable = inspect.isawaitable
    finite = math.isfinite
    encode_json = json.dumps

    class WorkpoolPipelineError(RuntimeError):
        """Failure with the pool, successful-reduction cursor and partial value."""

    async def resolve(value):
        return await value if is_awaitable(value) else value

    def invalid():
        return TypeError(
            "EVAL_PIPELINE_INPUT: expected an iterable, map/reduce callbacks "
            "and supported typed options"
        )

    def protocol():
        return RuntimeError("EVAL_PIPELINE_PROTOCOL: inconsistent native pool result")

    async def map_reduce(source, map_task, reduce, initial, *, pool=None,
                         max_items=32, timeout=30, raise_errors=True, on_pool=None,
                         ordered=False):
        """Reduce full native results as they settle, with bounded input demand.

        Both callbacks may be synchronous or async. `timeout` is shared across
        result waits and checked between callback/iterator steps; the cell's
        native watchdog, not a detached Python task, bounds guest code itself.
        `ordered=True` reduces in source order without serializing the workers.
        """
        if (not callable(map_task) or not callable(reduce)
                or type(max_items) is not int or not 1 <= max_items <= 32
                or type(timeout) not in (int, float) or not finite(timeout)
                or not 0 < timeout <= 600 or type(raise_errors) is not bool
                or type(ordered) is not bool
                or (on_pool is not None and not callable(on_pool))
                or (pool is not None and type(pool) is not dict)
                or isinstance(source, (str, bytes, bytearray))
                or (type(source) in (list, tuple) and len(source) > max_items)):
            raise invalid()
        pool_options = {} if pool is None else dict(pool)
        asynchronous = hasattr(source, "__aiter__")
        iterator = aiter(source) if asynchronous else iter(source)
        deadline = clock() + timeout
        carrier = None
        cursor = None
        # Receiving a result is not a successful reduction acknowledgment.
        # Keep this cursor private so buffered results remain recoverable.
        received = None
        value = initial
        admitted = completed = succeeded = failed = 0
        exhausted = sealed = False
        stage = "create"
        current = None
        pending = {}
        buffered = {}
        prepared = []

        def remaining():
            seconds = deadline - clock()
            if seconds <= 0:
                raise TimeoutError("EVAL_PIPELINE_TIMEOUT: shared pipeline wait budget exhausted")
            return min(seconds, 600)

        def progress():
            return {"value": value, "admitted": admitted, "completed": completed,
                    "succeeded": succeeded, "failed": failed}

        def cursor_after(previous, item_id):
            ids = [] if previous is None else list(previous["itemIds"])
            if item_id in ids:
                raise protocol()
            ids.append(item_id)
            return {"schema": "pi.subagent.pool.cursor.v1", "poolId": carrier.id,
                    "itemIds": sorted(ids)}

        async def pull():
            if asynchronous:
                try:
                    return False, await anext(iterator)
                except StopAsyncIteration:
                    return True, None
            try:
                return False, next(iterator)
            except StopIteration:
                # Do not allow StopIteration to escape an async function.
                return True, None

        try:
            carrier = await create_pool(**pool_options)
            remaining()
            concurrency = carrier.concurrency
            max_batch_size = carrier.max_batch_size
            if (type(concurrency) is not int or not 1 <= concurrency <= 8
                    or type(max_batch_size) is not int or not 1 <= max_batch_size <= 8):
                raise protocol()
            # Keep enough pending assignments for native grouped turns, while
            # only successful reduction frees admission capacity.
            window = min(32, max_items, concurrency * max_batch_size)
            if on_pool is not None:
                stage = "on_pool"
                await resolve(on_pool(carrier))
                remaining()

            async def fill(capacity=window):
                nonlocal exhausted, stage, current
                while not exhausted and len(pending) + len(prepared) < capacity:
                    stage = "source"
                    current = None
                    remaining()
                    exhausted, input_value = await pull()
                    remaining()
                    if exhausted:
                        break
                    # Probe exactly one extra value to detect capacity overflow;
                    # it is neither mapped nor silently dropped as success.
                    if admitted + len(prepared) == max_items:
                        raise RuntimeError(
                            "EVAL_PIPELINE_CAPACITY: source exceeds max_items; "
                            "accepted work was not replayed"
                        )
                    index = admitted + len(prepared)
                    current = {"index": index, "input": input_value}
                    stage = "map"
                    prompt = await resolve(map_task(input_value, index))
                    remaining()
                    if type(prompt) is not str or not prompt.strip():
                        raise invalid()
                    prepared.append((index, input_value, prompt))

            def batch_size():
                # Keep an encoded window inside the shared 256 KiB input cap,
                # including JSON escapes and the current acknowledgment cursor.
                arguments = {"operation": "pool_exchange", "poolId": carrier.id,
                             "tasks": [], "seal": False, "timeoutMs": 600000, "maxItems": 4}
                if received is not None:
                    arguments["cursor"] = received
                count = 0
                for _, _, prompt in prepared:
                    arguments["tasks"].append(prompt)
                    try:
                        encoded = encode_json({"name": "subagent", "args": arguments},
                                              ensure_ascii=False, allow_nan=False,
                                              separators=(",", ":")).encode("utf-8")
                    except (UnicodeError, ValueError):
                        break
                    if len(encoded) > 256 * 1024:
                        break
                    count += 1
                # Preserve the facade's error for an invalid individual task.
                return max(1, count) if prepared else 0

            def acknowledge(item_ids, count=None):
                nonlocal admitted
                count = len(prepared) if count is None else count
                if (type(item_ids) not in (list, tuple) or len(item_ids) != count
                        or any(type(item_id) is not int or item_id != admitted + offset + 1
                               for offset, item_id in enumerate(item_ids))):
                    raise protocol()
                for item_id, (index, input_value, _) in zip(item_ids, prepared):
                    pending[item_id] = (index, input_value)
                admitted += count
                del prepared[:count]

            async def reduce_one(item_id, outcome):
                nonlocal current, stage, value, cursor, completed, succeeded, failed
                current, ok = outcome
                stage = "result"
                remaining()
                if raise_errors and not ok:
                    raise RuntimeError("EVAL_PIPELINE_ITEM_FAILED: a native pool item did not succeed")
                stage = "reduce"
                value = await resolve(reduce(value, current))
                # Only a returned reducer commits progress. Cancellation or a
                # later failing result cannot acknowledge buffered outcomes.
                cursor = cursor_after(cursor, item_id)
                del pending[item_id]
                buffered.pop(item_id, None)
                completed += 1
                if ok:
                    succeeded += 1
                else:
                    failed += 1
                remaining()

            # Establish a native identity with the first item promptly. Later
            # bounded windows combine admission and observation in one host
            # call, so even 32 serial items fit the unchanged 64-call budget.
            await fill(1)
            if not prepared:
                return {**progress(), "pool": carrier, "cursor": cursor, "status": "empty"}
            stage = "admission"
            handles = await carrier.push([prepared[0][2]])
            if (type(handles) not in (list, tuple) or len(handles) != 1
                    or handles[0].pool_id != carrier.id):
                raise protocol()
            acknowledge([handles[0].item_id])
            remaining()
            await fill()
            while True:
                count = batch_size()
                sealing = exhausted and not sealed and count == len(prepared)
                if prepared or sealing:
                    stage = "admission" if prepared else "finish"
                    current = ({"index": prepared[count - 1][0], "input": prepared[count - 1][1]}
                               if count else None)
                    budget = remaining()
                    # Admit an oversized window in nonblocking bounded pieces
                    # before waiting, so worker concurrency is still available.
                    page = await carrier.exchange([entry[2] for entry in prepared[:count]],
                                                  cursor=received,
                                                  timeout=0 if count < len(prepared) else budget,
                                                  max_items=4, seal=sealing)
                    if type(page) is not dict:
                        raise protocol()
                    acknowledge(page.get("admittedItemIds"), count)
                    sealed |= sealing
                    remaining()
                else:
                    stage = "drain"
                    current = None
                    page = await carrier.drain(cursor=received, timeout=remaining(), max_items=4)
                if (type(page) is not dict or type(page.get("items")) is not list
                        or type(page.get("done")) is not bool or page.get("poolId") != carrier.id):
                    raise protocol()
                for item in page["items"]:
                    if (type(item) is not dict or type(item.get("id")) is not int
                            or item["id"] not in pending
                            or item.get("status") not in ("completed", "failed", "cancelled")
                            or type(item.get("result")) is not dict
                            or type(item["result"].get("content")) is not list
                            or "details" not in item["result"]):
                        raise protocol()
                    item_id = item["id"]
                    index, input_value = pending[item_id]
                    ok = item["status"] == "completed" and not item["result"].get("isError", False)
                    outcome = ({**item, "index": index, "input": input_value}, ok)
                    received = cursor_after(received, item_id)
                    if ordered:
                        buffered[item_id] = outcome
                        # Keep buffered outcomes pending: slow earlier work
                        # cannot open more than the existing admission window.
                        while completed + 1 in buffered:
                            next_id = completed + 1
                            await reduce_one(next_id, buffered[next_id])
                    else:
                        await reduce_one(item_id, outcome)
                if page["done"]:
                    if not exhausted or not sealed or pending:
                        raise protocol()
                    if raise_errors and page.get("status") != "completed":
                        stage = "finish"
                        raise RuntimeError("EVAL_PIPELINE_POOL_FAILED: native pool terminated unsuccessfully")
                    return {**progress(), "pool": carrier, "cursor": cursor, "status": page["status"]}
                if not page["items"] and not prepared:
                    raise TimeoutError("EVAL_PIPELINE_TIMEOUT: shared pipeline wait budget exhausted")
                await fill()
        except BaseException as cause:
            # Preserve asyncio cancellation, soft interrupts and process-level
            # control exceptions. Ordinary failures carry a recovery envelope.
            control = not isinstance(cause, Exception) or isinstance(cause, asyncio.CancelledError)
            error = cause if control else WorkpoolPipelineError(
                "EVAL_PIPELINE_FAILED: pipeline stopped; inspect cause, pool, cursor "
                "and progress before recovery"
            )
            error.stage = stage
            error.pool = carrier
            error.cursor = cursor
            error.progress = progress()
            error.item = current
            error.pending = tuple({"itemId": item_id, "index": record[0], "input": record[1]}
                                  for item_id, record in pending.items())
            # These mapped inputs have no trustworthy admission acknowledgment.
            # A lost exchange reply may mean they exist natively; never replay.
            error.prepared = tuple({"index": index, "input": input_value}
                                   for index, input_value, _ in prepared)
            error.cleanup_errors = []
            cleanup_control = None
            if carrier is not None:
                try:
                    error.cleanup = await carrier.close()
                except BaseException as cleanup_error:
                    error.cleanup_errors.append(cleanup_error)
                    if not isinstance(cleanup_error, Exception):
                        cleanup_control = cleanup_error
            if not exhausted:
                try:
                    close = getattr(iterator, "aclose" if asynchronous else "close", None)
                    if close is not None:
                        await resolve(close())
                except BaseException as cleanup_error:
                    error.cleanup_errors.append(cleanup_error)
                    if not isinstance(cleanup_error, Exception) and cleanup_control is None:
                        cleanup_control = cleanup_error
            if control:
                raise
            if cleanup_control is not None:
                # A new cancellation/interrupt during cleanup must not be
                # converted into an ordinary map/reduce failure.
                cleanup_control.pipeline_error = error
                raise cleanup_control from cause
            raise error from cause

    create_pool.map_reduce = map_reduce
