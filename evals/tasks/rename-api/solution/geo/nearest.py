"""Finding the nearest points."""

from . import measure


def nearest(origin, points):
    """The point closest to ``origin``; on a tie, the one listed first. ValueError if there are none."""
    if not points:
        raise ValueError("no points")
    return min(points, key=lambda p: measure.distance(origin, p))


def within(origin, points, radius):
    """The points at most ``radius`` from ``origin``, nearest first."""
    close = [p for p in points if measure.distance(origin, p) <= radius]
    return sorted(close, key=lambda p: measure.distance(origin, p))
