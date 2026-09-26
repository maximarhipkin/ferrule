import { Cart } from "./cart";

export const CartView = ({ cart }) => {
  return <Summary total={cart.checkoutTotal()} />;
};

function Summary({ total }) {
  return <div>{total.cents}</div>;
}
