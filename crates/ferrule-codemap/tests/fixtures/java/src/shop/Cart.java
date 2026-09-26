package shop;

import java.util.ArrayList;
import shop.Price;

public class Cart implements Checkout {
    private final ArrayList<Money> items = new ArrayList<>();

    public Cart() {}

    public void addItem(Money price) {
        items.add(price);
    }

    @Override
    public Money checkoutTotal() {
        long sum = 0;
        for (Money m : items) sum += m.cents;
        return Price.applyDiscount(new Money(sum), 10);
    }
}
