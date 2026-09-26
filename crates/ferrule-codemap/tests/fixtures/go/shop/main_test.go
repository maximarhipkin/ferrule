package shop

import "testing"

func TestCheckout(t *testing.T) {
	c := &Cart{}
	c.AddItem(NewMoney(100))
	if c.CheckoutTotal().Cents != 90 {
		t.Fatal("discount")
	}
}
