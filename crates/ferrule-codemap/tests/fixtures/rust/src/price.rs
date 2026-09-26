#[derive(Clone, Copy)]
pub struct Money {
    pub cents: i64,
}

impl Money {
    pub fn new(cents: i64) -> Self {
        Money { cents }
    }
}

pub fn apply_discount(total: Money, percent: i64) -> Money {
    Money::new(total.cents * (100 - percent) / 100)
}

pub enum Currency {
    Shekel,
    Euro,
}

macro_rules! money {
    ($c:expr) => {
        Money::new($c)
    };
}
