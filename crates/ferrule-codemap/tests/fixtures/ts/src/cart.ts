import { applyDiscount, Money } from "./price";

export interface Checkout {
  checkoutTotal(): Money;
}

export class Cart implements Checkout {
  private items: Money[] = [];

  addItem(price: Money): void {
    function localHelper() {}
    localHelper();
    this.items.push(price);
  }

  checkoutTotal(): Money {
    const sum = this.items.reduce((a, m) => a + m.cents, 0);
    return applyDiscount(new Money(sum), 10);
  }
}
