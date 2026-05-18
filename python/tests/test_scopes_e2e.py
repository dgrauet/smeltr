"""End-to-end: smeltr.scope emits ModuleEntered/ModuleReturned through the
daemon socket, with a matching call_id, in the right order, with the user's
name as `qualname` and `class_name="Scope"`.
"""

from __future__ import annotations

import smeltr


def _payload_kinds(received: list[dict]) -> list[str]:
    return [m["payload"]["kind"] for m in received if "payload" in m]


def test_scope_emits_module_entered_and_returned(fake_daemon):
    smeltr.attach()
    try:
        with smeltr.scope("denoise.pass:cond"):
            smeltr.mark("inside")
    finally:
        smeltr.detach()

    payloads = [m["payload"] for m in fake_daemon.received if "payload" in m]

    entered = [p for p in payloads if p["kind"] == "ModuleEntered"]
    returned = [p for p in payloads if p["kind"] == "ModuleReturned"]

    scope_entered = [p for p in entered if p.get("qualname") == "denoise.pass:cond"]
    assert len(scope_entered) == 1, f"expected one ModuleEntered for our scope, got {entered}"

    enter = scope_entered[0]
    assert enter["class_name"] == "Scope"
    assert enter["parent_call_id"] is None
    assert enter["depth"] == 0

    matching_return = [r for r in returned if r["module_call_id"] == enter["module_call_id"]]
    assert len(matching_return) == 1

    kinds = _payload_kinds(fake_daemon.received)
    enter_idx = kinds.index("ModuleEntered")
    return_idx = kinds.index("ModuleReturned", enter_idx)
    mark_idx = next(
        i for i, p in enumerate(payloads) if p["kind"] == "Mark" and "inside" in p["label"]
    )
    assert enter_idx < mark_idx < return_idx
