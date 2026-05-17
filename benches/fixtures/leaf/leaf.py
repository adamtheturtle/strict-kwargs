"""Minimal leaf module: a few local functions and calls, no imports.

Represents the cheapest realistic workload (issue #30): a tiny file whose
import closure is empty, so the measured cost is almost entirely the fixed
per-invocation work (parse, index, walk) rather than closure traversal.
"""


def add(left: int, right: int) -> int:
    return left + right


def scale(value: int, factor: int = 2) -> int:
    return value * factor


def describe(name: str, count: int) -> str:
    return f"{name}: {count}"


total = add(1, 2)
scaled = scale(total, 3)
label = describe("items", scaled)
