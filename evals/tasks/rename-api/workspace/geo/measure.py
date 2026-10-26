"""Measuring between points."""

import math


def calc_dist(a, b):
    """The straight-line distance between points a and b."""
    return math.hypot(b.x - a.x, b.y - a.y)


def bearing(a, b):
    """The direction from a to b in degrees, clockwise from north (the positive y axis)."""
    return math.degrees(math.atan2(b.x - a.x, b.y - a.y)) % 360
