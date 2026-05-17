"""Example file demonstrating strict-kwargs.

Run ``strict-kwargs example.py`` to see the flagged lines.
"""

from typing import Any

foo = str("a")


def add(a: int, b: int) -> int:
    return a + b


# OK: arguments passed by keyword.
add(a=1, b=2)

# FLAG: too many positional arguments (both could be keywords).
add(1, 2)

# FLAG: still positional even though one is keyword.
add(1, b=2)


def divide(a: int, /, b: int = 2) -> float:
    return a / b


# OK: ``a`` is positional-only, so passing it positionally is fine.
divide(1)
divide(1, b=4)

# FLAG: ``b`` is not positional-only.
divide(1, 4)


def make(*args: int, scale: int = 1) -> list[int]:
    return [x * scale for x in args]


# OK: *args absorbs positionals; ``scale`` given by keyword.
make(1, 2, 3, scale=2)


def kw_only(*, name: str, count: int = 0) -> str:
    return name * count


# OK: keyword-only parameters.
kw_only(name="hi", count=3)


class Counter:
    def __init__(self, *, start: int = 0) -> None:
        self.value = start

    def bump(self, step: int) -> None:
        self.value += step


c = Counter(start=10)

# OK: method call with keyword argument.
c.bump(step=5)

# FLAG: method call with positional argument (``self`` is excluded).
c.bump(1)


class Decorator:
    def __call__(self, func: Any) -> Any:
        return func


# OK: single-argument callable used as a decorator.
@Decorator()
def decorated() -> None: ...
