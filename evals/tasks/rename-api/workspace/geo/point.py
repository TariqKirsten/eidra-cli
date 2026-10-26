"""Points in the plane."""

from dataclasses import dataclass


@dataclass(frozen=True)
class Point:
    x: float
    y: float

    def moved(self, dx, dy):
        return Point(self.x + dx, self.y + dy)
