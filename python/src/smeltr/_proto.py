"""Wire protocol constants matching crates/smeltr-core/src/event.rs and
crates/smeltr-daemon/src/protocol.rs.
"""

from typing import Any

SOURCE_MARK = "Mark"
SOURCE_SYSTEM = "System"
SOURCE_PYTHON_SIDECAR = "PythonSidecar"

MAX_FRAME_BYTES = 16 * 1024 * 1024


def emit_msg(source: str, pid: int | None, payload: dict[str, Any]) -> dict[str, Any]:
    return {"op": "Emit", "source": source, "pid": pid, "payload": payload}


def hello_msg(client: str) -> dict[str, Any]:
    return {"op": "Hello", "client": client}
