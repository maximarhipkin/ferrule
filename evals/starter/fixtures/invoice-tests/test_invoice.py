import unittest

from invoice import total


class InvoiceTest(unittest.TestCase):
    def test_total_plain(self):
        self.assertEqual(total([(2, 50)]), 117.0)

    def test_total_with_discount(self):
        self.assertEqual(total([(2, 50)], discount=0.1), 105.3)

    def test_total_empty(self):
        self.assertEqual(total([]), 0.0)

    def test_discount_never_makes_it_negative(self):
        self.assertEqual(total([(1, 10)], discount=1.5), 0.0)


if __name__ == "__main__":
    unittest.main()
