"""Writes the shop package (long docstrings, so reading it fills the context)
and its tests. Deterministic."""
import os
import random

rng = random.Random(77)
os.makedirs("shop", exist_ok=True)


def notes(topic, n):
    words = ["rounding", "currency", "VAT", "bundle", "refund", "legacy", "catalog", "promotion",
             "invoice", "ledger", "warehouse", "partner", "export", "audit", "fixture"]
    out = []
    for i in range(n):
        out.append(f"    Note {i + 1} on {topic}: the {rng.choice(words)} path keeps the "
                   f"{rng.choice(words)} behaviour from the 2019 system, see ticket SHOP-{rng.randint(100, 999)}; "
                   f"changing it needs sign-off from {rng.choice(['finance', 'ops', 'legal'])}.")
    return "\n".join(out)


files = {
    "shop/__init__.py": '"""The shop package."""\n\nfrom .pricing import get_price\nfrom .tax import calc_tax\n'
                        'from .coupons import apply_coupon\n\n__all__ = ["get_price", "calc_tax", "apply_coupon"]\n',
    "shop/catalog.py": f'"""The product catalog.\n\n{notes("the catalog", 170)}\n"""\n\n'
                       'PRICES = {"mat": 120.0, "cover": 249.0, "hook": 15.5, "tray": 89.9}\n',
    "shop/pricing.py": f'"""Prices.\n\n{notes("pricing", 170)}\n"""\n\nfrom .catalog import PRICES\n\n\n'
                       'def get_price(sku, qty=1):\n    """The price of `qty` units of `sku`."""\n'
                       '    return round(PRICES[sku] * qty, 2)\n',
    "shop/tax.py": f'"""Tax.\n\n{notes("tax", 170)}\n"""\n\nfrom .pricing import get_price\n\nRATE = 0.17\n\n\n'
                   'def calc_tax(sku, qty=1):\n    """The VAT on `qty` units of `sku`."""\n'
                   '    return round(get_price(sku, qty) * RATE, 2)\n',
    "shop/coupons.py": f'"""Coupons.\n\n{notes("coupons", 170)}\n"""\n\nfrom .pricing import get_price\n\n'
                       'CODES = {"SPRING10": 0.10, "VIP25": 0.25}\n\n\n'
                       'def apply_coupon(sku, qty, code):\n    """The price after the coupon `code`."""\n'
                       '    return round(get_price(sku, qty) * (1 - CODES.get(code, 0)), 2)\n',
    "shop/cart.py": f'"""The cart.\n\n{notes("the cart", 150)}\n"""\n\nfrom .coupons import apply_coupon\nfrom .pricing import get_price\n'
                    'from .tax import calc_tax\n\n\ndef total(items, code=None):\n    """Sum of (sku, qty) items, with tax, after the coupon."""\n'
                    '    s = 0.0\n    for sku, qty in items:\n        base = apply_coupon(sku, qty, code) if code else get_price(sku, qty)\n'
                    '        s += base + calc_tax(sku, qty)\n    return round(s, 2)\n',
    "test_shop.py": 'import unittest\n\nfrom shop import apply_coupon, calc_tax, get_price\nfrom shop.cart import total\n\n\n'
                    'class ShopTest(unittest.TestCase):\n    def test_get_price(self):\n        self.assertEqual(get_price("hook", 2), 31.0)\n\n'
                    '    def test_calc_tax(self):\n        self.assertEqual(calc_tax("mat"), 20.4)\n\n'
                    '    def test_apply_coupon(self):\n        self.assertEqual(apply_coupon("cover", 1, "VIP25"), 186.75)\n\n'
                    '    def test_total(self):\n        self.assertEqual(total([("mat", 1), ("hook", 2)]), 176.67)\n\n\n'
                    'if __name__ == "__main__":\n    unittest.main()\n',
}
for path, text in files.items():
    with open(path, "w") as f:
        f.write(text)
