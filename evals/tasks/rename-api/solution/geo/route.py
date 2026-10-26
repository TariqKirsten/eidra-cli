"""Routes: points visited in order."""

from .measure import distance


def path_length(points):
    """The length of the route through ``points``, in order."""
    return sum(distance(a, b) for a, b in zip(points, points[1:]))


def legs(points):
    """Each leg of the route, as (start, end, length)."""
    return [(a, b, distance(a, b)) for a, b in zip(points, points[1:])]
