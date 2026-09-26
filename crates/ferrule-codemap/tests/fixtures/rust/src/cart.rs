use crate::price::{apply_discount, Money};

/// A shopping cart.
#[derive(Default)]
pub struct Cart {
    items: Vec<Item>,
}

pub struct Item {
    pub name: String,
    pub price: Money,
}

pub trait Checkout {
    fn checkout_total(&self) -> Money;
}

impl Cart {
    pub fn add_item(&mut self, name: &str, price: Money) {
        fn local_helper() {}
        local_helper();
        self.items.push(Item { name: name.into(), price });
    }
}

impl Checkout for Cart {
    fn checkout_total(&self) -> Money {
        let sum = self.items.iter().map(|i| i.price.cents).sum();
        apply_discount(Money::new(sum), 10)
    }
}
