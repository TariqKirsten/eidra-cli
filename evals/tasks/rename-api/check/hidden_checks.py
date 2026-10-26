import os
import unittest
import warnings

import geo
from geo import Point
from geo.measure import distance as measure_distance


class RenameTest(unittest.TestCase):
    def test_distance_is_the_function(self):
        self.assertEqual(geo.distance(Point(0, 0), Point(3, 4)), 5.0)
        self.assertIs(geo.distance, measure_distance)
        self.assertIn("distance", geo.__all__)

    def test_calc_dist_warns_from_the_callers_line_and_still_works(self):
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            self.assertEqual(geo.calc_dist(Point(0, 0), Point(3, 4)), 5.0)
        self.assertEqual(len(caught), 1)
        self.assertIs(caught[0].category, DeprecationWarning)
        self.assertEqual(os.path.abspath(caught[0].filename), os.path.abspath(__file__))

    def test_nothing_in_geo_uses_the_old_name(self):
        from geo.nearest import nearest, within
        from geo.route import legs, path_length

        points = [Point(0, 0), Point(3, 4), Point(6, 8)]
        with warnings.catch_warnings():
            warnings.simplefilter("error", DeprecationWarning)
            self.assertEqual(path_length(points), 10.0)
            self.assertEqual(legs(points)[0][2], 5.0)
            self.assertEqual(nearest(Point(0, 0), points), Point(0, 0))
            self.assertEqual(within(Point(0, 0), points, 6), [Point(0, 0), Point(3, 4)])
