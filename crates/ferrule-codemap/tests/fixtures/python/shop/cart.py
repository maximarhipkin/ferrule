from shop.price import apply_discount, Money


class Cart:
    def __init__(self):
        self.items = []

    def add_item(self, name, price: Money):
        def local_helper():
            pass
        local_helper()
        self.items.append((name, price))

    def checkout_total(self) -> Money:
        total = sum(p.cents for _, p in self.items)
        return apply_discount(Money(total), 10)
