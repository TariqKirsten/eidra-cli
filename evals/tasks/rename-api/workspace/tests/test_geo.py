import math
import unittest

from geo import Point, calc_dist, nearest, path_length
from geo.measure import bearing
from geo.nearest import within
from geo.route import legs


class GeoTest(unittest.TestCase):
    def test_calc_dist(self):
        self.assertEqual(calc_dist(Point(0, 0), Point(3, 4)), 5.0)
        self.assertEqual(calc_dist(Point(1, 1), Point(1, 1)), 0.0)

    def test_path_length(self):
        self.assertEqual(path_length([Point(0, 0), Point(3, 4), Point(6, 8)]), 10.0)
        self.assertEqual(path_length([Point(0, 0)]), 0)

    def test_legs(self):
        self.assertEqual(legs([Point(0, 0), Point(0, 2)]), [(Point(0, 0), Point(0, 2), 2.0)])

    def test_nearest(self):
        self.assertEqual(nearest(Point(0, 0), [Point(5, 5), Point(1, 1)]), Point(1, 1))
        with self.assertRaises(ValueError):
            nearest(Point(0, 0), [])

    def test_within(self):
        points = [Point(3, 0), Point(1, 0), Point(9, 9)]
        self.assertEqual(within(Point(0, 0), points, 5), [Point(1, 0), Point(3, 0)])

    def test_bearing(self):
        self.assertTrue(math.isclose(bearing(Point(0, 0), Point(1, 0)), 90))
