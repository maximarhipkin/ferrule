from dataclasses import dataclass

MAX_PERCENT = 100
lowercase_global = 1


@dataclass
class Money:
    cents: int


def apply_discount(total: Money, percent: int) -> Money:
    return Money(total.cents * (MAX_PERCENT - percent) // MAX_PERCENT)
