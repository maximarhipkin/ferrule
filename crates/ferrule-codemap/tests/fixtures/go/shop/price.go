package shop

const MaxPercent = 100

type Money struct {
	Cents int64
}

type Percent = int64

func NewMoney(cents int64) Money {
	const localConst = 1
	return Money{Cents: cents}
}

func ApplyDiscount(total Money, percent Percent) Money {
	return NewMoney(total.Cents * (MaxPercent - percent) / MaxPercent)
}
