"""Measuring between points."""

import math
import warnings


def distance(a, b):
    """The straight-line distance between points a and b."""
    return math.hypot(b.x - a.x, b.y - a.y)


def calc_dist(a, b):
    """Deprecated: use distance."""
    warnings.warn("calc_dist is deprecated; use distance", DeprecationWarning, stacklevel=2)
    return distance(a, b)


def bearing(a, b):
    """The direction from a to b in degrees, clockwise from north (the positive y axis)."""
    return math.degrees(math.atan2(b.x - a.x, b.y - a.y)) % 360
