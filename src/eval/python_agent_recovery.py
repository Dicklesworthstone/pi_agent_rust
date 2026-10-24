"""Recover native agent handles before the retained Python kernel accepts cells.

Reuse the kernel's protocol validators and concrete handle classes. Only native
identities and guest admission state are retained; every operation still calls
the current kernel bridge. No polling, process launcher, scheduler or authority
is added. This module is staged privately beside the compiled-in kernel.
"""


def install(kernel):
    """Install once at kernel startup, before any worker or pool handle exists."""
    if getattr(kernel, "_agent_recovery_installed", False):
        return

    base_worker = kernel._AgentWorker
    base_pool = kernel._AgentPool
    error_type = kernel.ToolBridgeError

    def identifier(value):
        if type(value) is not str or not value or len(value) > 128:
            return False
        try:
            return (len(value.encode("utf-8")) <= 128
                    and not any(char.isspace() or ord(char) < 32 or ord(char) == 127
                                for char in value))
        except UnicodeError:
            return False

    def turn_number(value):
        return type(value) is int and 1 <= value <= 32

    class AgentWorker(base_worker):
        __slots__ = ("_recovered", "_send_allowed", "_unknown_admission", "_pending_sends")

        def __init__(self, snapshot, *, allow_send=True, recovered=False):
            super().__init__(snapshot)
            self._recovered = recovered
            self._send_allowed = allow_send
            self._unknown_admission = False
            self._pending_sends = {}

        def _check_pending_sends(self):
            # The original cell may have closed while its send lacked a reply.
            # Retaining a handle never reclassifies that request as unsubmitted.
            if any(not owner.active or not owner.bridge_open
                   for owner in self._pending_sends.values()):
                self._unknown_admission = True
                self._send_allowed = False

        @property
        def recovered(self):
            return self._recovered

        @property
        def send_allowed(self):
            self._check_pending_sends()
            return self._send_allowed

        @property
        def unknown_admission(self):
            self._check_pending_sends()
            return self._unknown_admission

        async def turn(self, turn_id):
            if not turn_number(turn_id):
                raise TypeError("EVAL_AGENT_INPUT: turn requires an integer from 1 to 32")
            turn = kernel._AgentTurn(self, turn_id)
            await kernel._agent_read_worker(turn, "worker_status")
            return turn

        async def send(self, prompt):
            if type(prompt) is not str or not prompt.strip():
                raise TypeError("EVAL_AGENT_INPUT: send requires a nonempty prompt")
            if not self.send_allowed:
                raise error_type(
                    "EVAL_AGENT_RECOVERY_REQUIRED: sends are disabled on this handle; "
                    "inspect native turns before explicitly attaching with allow_send=True"
                )
            owner = kernel._bridge_owner()
            token = object()
            self._pending_sends[token] = owner
            try:
                # Keep the existing bridge, identity validation, concurrency
                # handling and fixed-turn result construction in one place.
                return await super().send(prompt)
            except BaseException:
                # Includes cancellation/soft timeout. Python's bridge owns
                # reply draining; do not consume, replace or retry that reply.
                self._unknown_admission = True
                self._send_allowed = False
                raise
            finally:
                self._pending_sends.pop(token, None)

    class AgentPool(base_pool):
        __slots__ = ("_recovered",)

        def __init__(self, options, name, concurrency):
            super().__init__(options, name, concurrency)
            self._recovered = False

        @property
        def recovered(self):
            return self._recovered

    async def attach_worker(worker_id, *, turn_id=None, allow_send=False):
        if (not identifier(worker_id) or type(allow_send) is not bool
                or (turn_id is not None and not turn_number(turn_id))):
            raise TypeError("EVAL_AGENT_INPUT: attachment requires a native worker ID and typed options")
        arguments = {"operation": "worker_status", "workerId": worker_id}
        if turn_id is not None:
            arguments["turnId"] = turn_id
        snapshot = kernel._agent_worker_snapshot(
            await kernel._agent_native_result("subagent", arguments), worker_id, turn_id,
        )
        if not identifier(snapshot["jobId"]):
            raise error_type("EVAL_AGENT_PROTOCOL: invalid recovered worker identity")
        return AgentWorker(snapshot, allow_send=allow_send, recovered=True)

    async def attach_pool(pool_id, *, allow_push=False):
        if not identifier(pool_id) or type(allow_push) is not bool:
            raise TypeError("EVAL_POOL_INPUT: attachment requires a native pool ID and boolean allow_push")
        snapshot = kernel._pool_snapshot(
            await kernel._agent_native_result("subagent", {
                "operation": "pool_status", "poolId": pool_id,
            }), pool_id,
        )
        if not identifier(snapshot["jobId"]) or not kernel._pool_bounded_text(snapshot["agent"], 128):
            raise error_type("EVAL_POOL_PROTOCOL: invalid recovered pool identity")
        pool = AgentPool({"agent": snapshot["agent"]}, "recovered-workpool", snapshot["concurrency"])
        pool._adopt(snapshot)
        pool._recovered = True
        pool._closed |= not allow_push
        pool._finish_acknowledged = snapshot["sealed"]
        return pool

    # Native factories and mixed waits resolve these classes from the kernel's
    # globals. Install before main: all subsequently created handles, including
    # ordinary fresh workers, get the same send-acknowledgment fence. The base
    # methods retain their existing kernel globals, protocol and bridge paths.
    kernel._AgentWorker = AgentWorker
    kernel._AgentPool = AgentPool
    kernel._agent.attach_worker = attach_worker
    kernel._workpool.attach = attach_pool
    kernel._agent_recovery_installed = True
