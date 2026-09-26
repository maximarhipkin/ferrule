package shop

type Cart struct {
	items []Money
}

type Checkout interface {
	CheckoutTotal() Money
}

func (c *Cart) AddItem(price Money) {
	c.items = append(c.items, price)
}

func (c *Cart) CheckoutTotal() Money {
	var sum int64
	for _, m := range c.items {
		sum += m.Cents
	}
	return ApplyDiscount(NewMoney(sum), 10)
}
