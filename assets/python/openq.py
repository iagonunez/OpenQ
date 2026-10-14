"""OpenQ effect API for scripts.

A script generates custom lighting effects: curves sampled over one cycle, one curve per attribute,
optionally a different curve for each position of a pattern. It runs in OpenQ's sandbox, which
has no files, network or other programs: the only thing a script can do is compute and return
effects. OpenQ then plays them like any other effect (speed, size, phase spread and so on still
apply) and can save them as effect files to share.

Levels are 0.0..1.0. Attribute names are case-insensitive and may use underscores.

    import openq
    fx = openq.effect("Rainbow", attributes=["red", "green", "blue"], steps=64, width=8)
    fx.fill(lambda t, row, width: openq.hsv(t + row / width, 1, 1))
"""

import colorsys
import math

__all__ = [
    "ATTRIBUTES", "Effect", "effect",
    "hsv", "lerp", "clamp", "smoothstep", "sine", "triangle", "square", "saw",
]

ATTRIBUTES = [
    "Dimmer", "Red", "Green", "Blue", "White", "Amber", "Lime", "Uv", "Cyan", "Magenta", "Yellow", "Cto",
    "ColorWheel", "Pan", "Tilt", "PanTiltSpeed", "Zoom", "Focus", "Iris", "Frost", "Gobo", "GoboRotation",
    "Prism", "Shutter", "Control", "Effect", "EffectSpeed", "Fog", "Fan",
]
"""Every attribute name, as OpenQ stores them."""

MAX_EFFECTS = 64
MAX_ATTRIBUTES = 16
MAX_STEPS = 4096
MAX_WIDTH = 512
MAX_VALUES = 1 << 20

_ATTR = {a.lower(): a for a in ATTRIBUTES}
_made = []


def _attr(name):
    k = str(name).replace("_", "").replace(" ", "").lower()
    if k not in _ATTR:
        raise ValueError(f"unknown attribute {name!r} (see openq.ATTRIBUTES)")
    return _ATTR[k]


def _level(v):
    v = float(v)
    if not math.isfinite(v):
        raise ValueError("levels must be finite numbers")
    return min(1.0, max(0.0, v))


class Effect:
    """A custom effect being generated. Create it with openq.effect()."""

    def __init__(self, name, attributes, steps, width, smooth):
        attrs = [_attr(a) for a in ([attributes] if isinstance(attributes, str) else attributes)]
        if not 1 <= len(attrs) <= MAX_ATTRIBUTES:
            raise ValueError(f"1 to {MAX_ATTRIBUTES} attributes")
        if len(set(attrs)) != len(attrs):
            raise ValueError("an attribute appears twice")
        steps, width = int(steps), int(width)
        if not 1 <= steps <= MAX_STEPS:
            raise ValueError(f"steps must be 1 to {MAX_STEPS}")
        if not 1 <= width <= MAX_WIDTH:
            raise ValueError(f"width must be 1 to {MAX_WIDTH}")
        if len(attrs) * steps * width > MAX_VALUES:
            raise ValueError(f"too many values (attributes × steps × width > {MAX_VALUES})")
        self.name = str(name)
        self.attributes = attrs
        self.steps = steps
        self.width = width
        self.smooth = bool(smooth)
        self.values = [0.0] * (len(attrs) * steps * width)
        # Playback settings the effect starts with (all can be changed in OpenQ afterwards).
        self.speed = 0.5
        """Cycles per second (per beat with sync)."""
        self.sync = False
        self.size = 1.0
        self.base = 0.0
        self.relative = False
        """True: around the cue's value; False: absolute (from base, by size)."""
        self.spread = 0.0
        """Phase offset across fixtures in degrees, on top of the pattern."""
        self.groups = 1
        self.wings = 1

    def _index(self, row, step, attr):
        col = self.attributes.index(_attr(attr)) if not isinstance(attr, int) else attr
        if not (0 <= row < self.width and 0 <= step < self.steps and 0 <= col < len(self.attributes)):
            raise IndexError("row, step or attribute out of range")
        return (row * len(self.attributes) + col) * self.steps + step

    def set(self, row, step, attr, value):
        """Set one sample: pattern row, step in the cycle, attribute (name or column)."""
        self.values[self._index(int(row), int(step), attr)] = _level(value)

    def get(self, row, step, attr):
        """One sample."""
        return self.values[self._index(int(row), int(step), attr)]

    def fill(self, fn):
        """Fill every sample from fn(t, row, width): t is the position in the cycle (0 ≤ t < 1).

        fn returns a number (one attribute), a tuple/list in attribute order, or a dict
        {attribute: level} (missing attributes stay 0). Returns the effect.
        """
        n = len(self.attributes)
        for row in range(self.width):
            for step in range(self.steps):
                v = fn(step / self.steps, row, self.width)
                if isinstance(v, dict):
                    vals = [0.0] * n
                    for a, x in v.items():
                        vals[self.attributes.index(_attr(a))] = x
                elif isinstance(v, (list, tuple)):
                    if len(v) != n:
                        raise ValueError(f"fn returned {len(v)} values for {n} attributes")
                    vals = v
                else:
                    if n != 1:
                        raise ValueError(f"fn returned one number for {n} attributes")
                    vals = (v,)
                for col, x in enumerate(vals):
                    self.values[(row * n + col) * self.steps + step] = _level(x)
        return self

    def _json(self):
        return {
            "name": self.name, "shape": "Custom", "attribute": self.attributes[0],
            "speed": float(self.speed), "sync": bool(self.sync), "size": _level(self.size), "base": _level(self.base),
            "relative": bool(self.relative), "spread": float(self.spread), "groups": int(self.groups),
            "wings": int(self.wings),
            "table": {"attributes": self.attributes, "steps": self.steps, "width": self.width,
                      "smooth": self.smooth, "values": self.values},
        }

    def __repr__(self):
        return f"Effect({self.name!r}, {self.attributes}, steps={self.steps}, width={self.width})"


def effect(name, attributes="dimmer", steps=64, width=1, smooth=True):
    """Start a custom effect; every effect a script creates is returned to OpenQ.

    attributes: one name or a list. steps: samples per cycle. width: pattern rows (fixture i
    plays row i % width). smooth: interpolate between samples (False holds them, for chases).
    """
    if len(_made) >= MAX_EFFECTS:
        raise ValueError(f"at most {MAX_EFFECTS} effects per script")
    e = Effect(name, attributes, steps, width, smooth)
    _made.append(e)
    return e


# --- Helpers -----------------------------------------------------------------------------------

def hsv(h, s=1.0, v=1.0):
    """(red, green, blue) for a hue (0..1, wraps), saturation and value."""
    return colorsys.hsv_to_rgb(h % 1.0, clamp(s), clamp(v))


def lerp(a, b, t):
    """a + (b - a) · t."""
    return a + (b - a) * t


def clamp(v, lo=0.0, hi=1.0):
    return min(hi, max(lo, v))


def smoothstep(t):
    """Eased 0..1 (slow start and end)."""
    t = clamp(t)
    return t * t * (3 - 2 * t)


def sine(t):
    """0 → 1 → 0 over one cycle, smooth."""
    return 0.5 - 0.5 * math.cos(2 * math.pi * t)


def triangle(t):
    """0 → 1 → 0 over one cycle, linear."""
    return 1.0 - abs(2.0 * (t % 1.0) - 1.0)


def square(t, duty=0.5):
    """1 for the first `duty` of the cycle, then 0."""
    return 1.0 if t % 1.0 < duty else 0.0


def saw(t):
    """Ramp 0 → 1, then snap back."""
    return t % 1.0
