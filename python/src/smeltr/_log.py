"""Lazy logging.

`import logging` costs ~8 ms, and autoload runs in every Python process
`smeltr record` starts (#266); only a failure needs it.
"""

from __future__ import annotations


def warning(logger: str, msg: str, *args: object) -> None:
    import logging

    logging.getLogger(logger).warning(msg, *args)
