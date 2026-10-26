# geo

Plane geometry for small route-planning jobs.

```python
from geo import Point, distance, nearest, path_length

distance(Point(0, 0), Point(3, 4))                     # 5.0
path_length([Point(0, 0), Point(3, 4), Point(6, 8)])   # 10.0
nearest(Point(0, 0), [Point(5, 5), Point(1, 1)])       # Point(x=1, y=1)
```

## Functions

- `distance(a, b)`: the straight-line distance between two points.
- `path_length(points)`: the length of a route through the points, in order.
- `nearest(origin, points)`: the point closest to `origin`.
- `geo.route.legs(points)`: each leg of a route, with its length.
- `geo.nearest.within(origin, points, radius)`: the points within `radius`, nearest first.
- `geo.measure.bearing(a, b)`: the direction from `a` to `b`, in degrees clockwise from north.

`calc_dist` is deprecated: it still works, with a DeprecationWarning, as another name for
`distance`.

Run `python3 examples/demo.py` for a short tour, and the tests with `python3 -m unittest -q`.
