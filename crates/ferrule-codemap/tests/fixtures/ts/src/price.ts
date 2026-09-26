export class Money {
  constructor(public cents: number) {}
}

export type Percent = number;

export enum Currency {
  Shekel,
  Euro,
}

export const MAX_PERCENT = 100;

export function applyDiscount(total: Money, percent: Percent): Money {
  return new Money((total.cents * (MAX_PERCENT - percent)) / MAX_PERCENT);
}
