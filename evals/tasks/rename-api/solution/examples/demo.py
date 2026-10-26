"""A short tour of geo: python3 examples/demo.py"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

import geo  # noqa: E402
from geo.route import legs  # noqa: E402

depot = geo.Point(0, 0)
stops = [geo.Point(3, 4), geo.Point(6, 8), geo.Point(-2, 1)]

print(f"depot to the first stop: {geo.distance(depot, stops[0]):.2f}")
print(f"the whole round trip: {geo.path_length([depot, *stops, depot]):.2f}")
print(f"the stop nearest the depot: {geo.nearest(depot, stops)}")
for start, end, length in legs([depot, *stops]):
    print(f"  {start} -> {end}: {length:.2f}")
