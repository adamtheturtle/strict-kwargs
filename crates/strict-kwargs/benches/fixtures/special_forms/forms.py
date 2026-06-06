"""Special-form and overload heavy module.

Stresses the overload-permissive signature model and the typing
special-form exemption (issue #30): many ``@overload`` definitions per
callee plus the PEP 484/612/646/695 special forms the checker must resolve
and then deliberately not flag.
"""

from typing import (
    Generic,
    NewType,
    ParamSpec,
    TypeVar,
    TypeVarTuple,
    Unpack,
    overload,
)

T = TypeVar("T")
P = ParamSpec("P")
Ts = TypeVarTuple("Ts")
UserId = NewType("UserId", int)


class Container(Generic[T]):
    @overload
    def get(self, key: int) -> T: ...
    @overload
    def get(self, key: str) -> T: ...
    @overload
    def get(self, key: int, default: T) -> T: ...
    @overload
    def get(self, key: str, default: T) -> T: ...
    def get(self, key, default=None):
        return default

    @overload
    def put(self, value: T) -> None: ...
    @overload
    def put(self, value: T, index: int) -> None: ...
    def put(self, value, index=0):
        self._value = value


class Tuple(Generic[Unpack[Ts]]):
    def __init__(self, *items: Unpack[Ts]) -> None:
        self.items = items


def make_user(raw: int) -> UserId:
    return UserId(raw)


container: Container[int] = Container()
container.get(1)
container.get("k")
container.get(1, 99)
container.put(5)
container.put(5, 2)

bundle = Tuple(1, "a", 3.0)
user = make_user(7)
