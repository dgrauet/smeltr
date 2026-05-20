"""Canonical smoke-test workload for smeltr.

Covers the surfaces exercised by GitHub issues #19, #31, #38, #40, #43,
#46, #47 plus the PR1/PR2/PR3 safetensors-load tracker. Runs as a
one-shot Python program — no CLI args. The companion script
`scripts/smoke-test.sh` invokes this under `smeltr record` and inspects
the resulting session.

Workload shape:
- 3 named scopes with structured `**fields` (testing #43, #46).
- One scope contains 3 nested inner scopes with distinct field values
  (testing scope-field discrimination from #46).
- Each scope contains at least one `mx.eval` (so it allocates GPU
  memory + emits CB events).
- One pass-through scope with no eval (tests #47 — synchronous mem
  sample on enter/exit must still register).
- Final `mark()` with structured fields (testing the v0.4.2 mark
  refactor).
- Deliberate duplicate safetensors load — same canonical path loaded
  twice via `mx.load` to exercise the v0.6.0 ModelLoad event,
  duplicate-model-load analyzer rule, and chrome-trace counter track.
"""

from __future__ import annotations

import os
import tempfile

os.environ.setdefault("SMELTR_STACK_CAPTURE", "1")

import smeltr
import mlx.core as mx


def main() -> None:
    # Sanity: smeltr was auto-attached by `smeltr record` via the
    # SMELTR_AUTOLOAD=1 .pth shim. Verify by reading the cached token.
    # (Not strictly needed; the smoke-test.sh script confirms attach.)

    with smeltr.scope("denoise.step", step=5, sigma=0.5):
        a = mx.random.uniform(shape=(512, 512))
        b = mx.random.uniform(shape=(512, 512))
        mx.eval(a @ b)

    with smeltr.scope("outer.loop", iteration=0):
        for i in range(3):
            with smeltr.scope("inner.pass", pass_idx=i, kind="matmul"):
                c = mx.random.uniform(shape=(256, 256))
                d = mx.random.uniform(shape=(256, 256))
                mx.eval(c @ d)

    # Pass-through scope with no mx.eval — tests #47 synchronous samples.
    with smeltr.scope("typed", layer=3, fp_dtype="bfloat16", causal=True):
        pass

    # PR1/2/3: write a tiny safetensors file and load it twice from the
    # same path — should produce 2 ModelLoad events + 1 duplicate finding.
    model_path = os.path.join(tempfile.gettempdir(), "smoke-model.safetensors")
    mx.save_safetensors(model_path, {"w": mx.random.uniform(shape=(64, 64))})
    _ = mx.load(model_path)
    _ = mx.load(model_path)

    # Structured mark (v0.4.2): label + fields.
    smeltr.mark("smoke-checkpoint", phase="final", ok=True)

    print("smoke_workload: OK")


if __name__ == "__main__":
    main()
