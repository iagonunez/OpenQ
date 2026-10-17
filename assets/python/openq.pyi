"""Type stub for OpenQ's effect API (`import openq` in an effect script).

Put this file where your editor looks for stubs (or next to your script) for completion.
Levels are 0.0..1.0; attribute names are case-insensitive and may use underscores.
"""

from typing import Callable, Iterable, Mapping, Sequence, Union

Levels = Union[float, Sequence[float], Mapping[str, float]]

ATTRIBUTES: list[str]

class Effect:
    """A custom effect being generated."""
    name: str
    attributes: list[str]
    steps: int
    width: int
    smooth: bool
    values: list[float]
    speed: float
    sync: bool
    size: float
    base: float
    relative: bool
    spread: float
    groups: int
    wings: int
    def set(self, row: int, step: int, attr: Union[str, int], value: float) -> None: ...
    def get(self, row: int, step: int, attr: Union[str, int]) -> float: ...
    def fill(self, fn: Callable[[float, int, int], Levels]) -> "Effect": ...

def effect(name: str, attributes: Union[str, Iterable[str]] = "dimmer", steps: int = 64, width: int = 1,
           smooth: bool = True) -> Effect: ...

def param(name: str, default: float = 0.5, min: float = 0.0, max: float = 1.0, integer: bool = False) -> float:
    """A setting shown as a slider in OpenQ; returns its current value."""
    ...

def hsv(h: float, s: float = 1.0, v: float = 1.0) -> tuple[float, float, float]: ...
def lerp(a: float, b: float, t: float) -> float: ...
def clamp(v: float, lo: float = 0.0, hi: float = 1.0) -> float: ...
def smoothstep(t: float) -> float: ...
def sine(t: float) -> float: ...
def triangle(t: float) -> float: ...
def square(t: float, duty: float = 0.5) -> float: ...
def saw(t: float) -> float: ...
