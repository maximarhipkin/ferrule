package shop;

public final class Price {
    public static Money applyDiscount(Money total, long percent) {
        return new Money(total.cents * (100 - percent) / 100);
    }
}

record Money(long cents) {}

enum Currency { SHEKEL, EURO }
