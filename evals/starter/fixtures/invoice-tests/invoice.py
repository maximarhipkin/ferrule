"""Invoice totals."""

TAX = 0.17


def line_total(qty, unit_price):
    return qty * unit_price


def total(lines, discount=0.0, tax=TAX):
    """The invoice total: the lines, less the discount (a fraction, 0.1 is
    10%), plus tax."""
    subtotal = sum(line_total(q, p) for q, p in lines)
    return round(subtotal * (1 + tax) - discount, 2)
