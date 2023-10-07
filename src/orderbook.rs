use std::{
    cmp::Reverse,
    collections::{btree_map::Entry, BTreeMap, VecDeque},
    fmt::Display,
    ops::{Deref, DerefMut},
};

use anyhow::Result;
use indexmap::IndexMap;
use num::Zero;
use rust_decimal::Decimal;
use thiserror::Error;

use crate::{
    order::{Order, OrderFeatures, OrderId, OrderPrice, OrderQuantity, OrderSide},
    trade::{Trade, TradeError, TradeId},
};

const DEFAULT_LEVEL_SIZE: usize = 8;

trait Ladder: Deref + DerefMut {
    fn insert(&mut self, order: &Order) -> Result<&mut Self, OrderbookError>;

    fn remove(&mut self, order: &Order) -> Result<&mut Self, OrderbookError>;
}

#[derive(Default)]
struct LadderWrapper<T>(T);

impl<T> Deref for LadderWrapper<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for LadderWrapper<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<K: Ord> LadderWrapper<BTreeMap<K, PriceLevel>> {
    fn peek_top<'a>(&'a self, orders: &'a IndexMap<OrderId, Order>) -> Option<&'a Order> {
        self.first_key_value()
            .map(|(_, level)| level)?
            .front()
            .and_then(|order| orders.get(& order.id()))
    }
}

impl Ladder for LadderWrapper<BTreeMap<OrderPrice, PriceLevel>> {
    fn insert(&mut self, order: &Order) -> Result<&mut Self, OrderbookError> {
        let limit_price = order
            .limit_price()
            .ok_or(OrderbookError::OrderToInsertWithNoLimitPrice(*order))?;
        let price_level = self
            .0
            .entry(limit_price)
            .or_insert_with(|| PriceLevel::new(limit_price));

        price_level.quantity += order.remaining();
        price_level.push_back(*order);

        Ok(self)
    }

    fn remove(&mut self, order: &Order) -> Result<&mut Self, OrderbookError> {
        let limit_price = order
            .limit_price()
            .ok_or(OrderbookError::OrderToRemoveWithNoLimitPrice(*order))?;
        let Entry::Occupied(mut price_level) = self.0.entry(limit_price) else {
            unreachable!();
        };

        if price_level.get().len() == 1 {
            price_level.remove();
        } else {
            let price_level = price_level.get_mut();
            price_level.quantity -= order.remaining();
            if let Some(idx) = price_level.iter().position(|&order_id| order.id() == order_id.id()) {
                price_level.remove(idx);
            }
        }

        Ok(self)
    }
}

impl Ladder for LadderWrapper<BTreeMap<Reverse<OrderPrice>, PriceLevel>> {
    fn insert(&mut self, order: &Order) -> Result<&mut Self, OrderbookError> {
        let limit_price = order
            .limit_price()
            .ok_or(OrderbookError::OrderToInsertWithNoLimitPrice(*order))?;
        let price_level = self
            .0
            .entry(Reverse(limit_price))
            .or_insert_with(|| PriceLevel::new(limit_price));

        price_level.quantity += order.remaining();
        price_level.push_back(*order);

        Ok(self)
    }

    fn remove(&mut self, order: &Order) -> Result<&mut Self, OrderbookError> {
        let limit_price = order
            .limit_price()
            .ok_or(OrderbookError::OrderToRemoveWithNoLimitPrice(*order))?;
        let Entry::Occupied(mut price_level) = self.0.entry(Reverse(limit_price)) else {
            unreachable!();
        };

        if price_level.get().len() == 1 {
            price_level.remove();
        } else {
            let price_level = price_level.get_mut();
            price_level.quantity -= order.remaining();
            if let Some(idx) = price_level.iter().position(|&order_id| order.id() == order_id.id()) {
                price_level.remove(idx);
            }
        }

        Ok(self)
    }
}

type AsksLadder = LadderWrapper<BTreeMap<OrderPrice, PriceLevel>>;
type BidsLadder = LadderWrapper<BTreeMap<Reverse<OrderPrice>, PriceLevel>>;

#[derive(Debug)]
pub struct PriceLevel {
    orders: VecDeque<Order>,
    quantity: OrderQuantity,
    price: OrderPrice,
}

impl PriceLevel {
    fn new(price: OrderPrice) -> Self {
        Self {
            orders: VecDeque::with_capacity(DEFAULT_LEVEL_SIZE),
            quantity: Decimal::ZERO,
            price,
        }
    }
}

impl Deref for PriceLevel {
    type Target = VecDeque<Order>;

    fn deref(&self) -> &Self::Target {
        //&self.order_ids
        &self.orders
    }
}

impl DerefMut for PriceLevel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.orders
    }
}

impl PriceLevel {
    #[inline]
    fn is_closed(&self) -> bool {
        self.quantity == OrderQuantity::zero()
    }

    #[inline]
    pub fn can_trade(&self, order: &Order) -> bool {
        self.quantity.min(order.remaining()) != OrderQuantity::ZERO
    }

    #[inline]
    pub fn matches(&self, order: &Order) -> bool {
        let level = self;

        if level.is_closed() || order.is_closed() {
            return false;
        }

        let level_limit_price = self.price;
        match order.limit_price() {
            // limit price == limit order
            Some(order_limit_price) => match order.side() {
                OrderSide::Ask => order_limit_price <= level_limit_price,
                OrderSide::Bid => order_limit_price >= level_limit_price,
            },
            None => true, // no limit price == market order
        }
    }
}

impl Display for PriceLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{:?}]", self.quantity, self.orders)
    }
}

macro_rules! match_order {
    ($incoming_order:ident, $orders:ident, $trades:ident, $order_ladder:ident, $opposite_ladder:ident) =>  {
        'exit: {
        // PostOnly orders should go directly to the book; otherwise, if they can be matched inmediately, then they should be canceled
        if $incoming_order.is_post_only()
            && $opposite_ladder
                .peek_top($orders)
                .is_some_and(|top_order| $incoming_order.matches(top_order))
        {
            $incoming_order.cancel();
            break 'exit Ok(false);
        }

        // FOK orders should be canceled if they cannot be fill completely
        if $incoming_order.is_fill_or_kill() {
            let mut can_be_filled = false;
            let mut remaining = $incoming_order.remaining();
                for (_, price_level) in $opposite_ladder.iter_mut() {
                if $incoming_order.is_closed() || !price_level.matches(& $incoming_order) {
                    break;
                }
                remaining -= price_level.quantity;
                if (remaining <= Decimal::ZERO) {
                    can_be_filled = true;
                    break;
                }
            }
            if !can_be_filled {
                $incoming_order.cancel();
                break 'exit Ok(false);
            }
        }

        let mut matched = false;
        let mut trades: Vec<Trade> = vec![];
        let mut drained_levels = 0;

        for (_, price_level) in $opposite_ladder.iter_mut() {
            if $incoming_order.is_closed() || !price_level.matches(&$incoming_order) {
                break;
            }

            let mut total_traded = OrderQuantity::ZERO;
            let mut orders_completed = 0;

            for maker in price_level.iter_mut() {
               // let maker = maker_order;
      
                let traded = $incoming_order.can_trade(maker);

                let trade = Trade::new(&mut $incoming_order, maker, traded).map_err(OrderbookError::TradeError)?;
                trades.push(trade);

                matched = true;

                total_traded += traded;
                if maker.is_closed() {
                    orders_completed += 1;
                }
            }

            price_level.quantity -= total_traded;
            for _ in 0..orders_completed {
                price_level.pop_front().and_then(|order| $orders.remove(& order.id()));
            }

            if price_level.quantity == OrderQuantity::ZERO {
                drained_levels += 1;
            }
        }
        for _ in 0..drained_levels {
            $opposite_ladder.pop_first();
        }

        // save trades
        for trade in trades {
            $trades.insert(trade.id(), trade);
        }

        // IOC orders should be closed at the end of the matching phase (this is, no insertion in the book)
        if $incoming_order.is_immediate_or_cancel() {
            $incoming_order.cancel();
            break 'exit Ok(matched);
        }

        // insert limit order in the book
        if !$incoming_order.is_closed() && $incoming_order.is_bookable() {
            $order_ladder.insert(&$incoming_order)?;
            $orders.insert($incoming_order.id(), $incoming_order);
        }

        Ok(matched)
    }};
}

#[derive(Default)]
pub struct Orderbook {
    asks: AsksLadder,
    bids: BidsLadder,
    orders: IndexMap<OrderId, Order>,
    trades: IndexMap<TradeId, Trade>,
}

type MatchResult = Result<bool, OrderbookError>;
#[cfg(test)]
const MATCHED: MatchResult = Ok(true);
#[cfg(test)]
const NOT_MATCHED: MatchResult = Ok(false);

type CancelResult = Result<Order, OrderbookError>;

impl Orderbook {
    #[inline]
    pub fn peek_top(&self, side: &OrderSide) -> Option<&Order> {
        match side {
            OrderSide::Ask => self.asks.peek_top(&self.orders),
            OrderSide::Bid => self.bids.peek_top(&self.orders),
        }
    }

    #[inline]
    pub fn handle_create(&mut self, mut order: Order) -> MatchResult {
        if self.orders.contains_key(&order.id()) {
            return Err(OrderbookError::OrderDuplicated(order.id()));
        }

        let orders = &mut self.orders;
        let trades = &mut self.trades;

        match order.side() {
            OrderSide::Ask => {
                let order_ladder = &mut self.asks;
                let opposite_ladder = &mut self.bids;
                match_order!(order, orders, trades, order_ladder, opposite_ladder)
            }
            OrderSide::Bid => {
                let order_ladder = &mut self.bids;
                let opposite_ladder = &mut self.asks;
                match_order!(order, orders, trades, order_ladder, opposite_ladder)
            }
        }
    }

    #[inline]
    pub fn handle_cancel(&mut self, order_id: OrderId) -> CancelResult {
        let order = self
            .orders
            .remove(&order_id)
            .ok_or(OrderbookError::OrderToCancelNotFound(order_id))?;

        match order.side() {
            OrderSide::Ask => {
                let order_ladder = &mut self.asks;
                order_ladder.remove(&order)?;
            }
            OrderSide::Bid => {
                let order_ladder = &mut self.bids;
                order_ladder.remove(&order)?;
            }
        }

        Ok(order)
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum OrderbookError {
    #[error("an order with the same ID has been handled before! {0}")]
    OrderDuplicated(OrderId),
    #[error("order cannot be inserted into the book with no limit price! {0}")]
    OrderToInsertWithNoLimitPrice(Order),
    #[error("order cannot be removed from the book with no limit price! {0}")]
    OrderToRemoveWithNoLimitPrice(Order),
    #[error("order to cancel not found in the book! {0}")]
    OrderToCancelNotFound(OrderId),
    #[error("order to match not found in the book! {0}")]
    OrderToMatchNotFound(OrderId),
    #[error("trade error: {0}")]
    TradeError(#[from] TradeError),
}

#[cfg(test)]
mod test {
    use rstest::{fixture, rstest};

    use super::*;
    use crate::order::{Order, OrderSide};

    // convention for order ids: 3-digit side (bid = 900, ask = 901), 3-digit quantity, 3-digit price (for market orders always 999)

    #[fixture]
    fn orderbook() -> Orderbook {
        Orderbook::default()
    }

    #[fixture]
    fn ask_100_at_015() -> Order {
        let order_id = OrderId::new(901_100_015);
        Order::limit_order(order_id, OrderSide::Ask, 100.into(), 15.into())
    }

    #[fixture]
    fn ask_080_at_015() -> Order {
        let order_id = OrderId::new(901_080_015);
        Order::limit_order(order_id, OrderSide::Ask, 80.into(), 15.into())
    }

    #[fixture]
    fn ask_070_at_014() -> Order {
        let order_id = OrderId::new(901_070_014);
        Order::limit_order(order_id, OrderSide::Ask, 70.into(), 14.into())
    }

    #[fixture]
    fn bid_025_at_014() -> Order {
        let order_id = OrderId::new(900_025_014);
        Order::limit_order(order_id, OrderSide::Bid, 25.into(), 14.into())
    }

    #[fixture]
    fn bid_099_at_015() -> Order {
        let order_id = OrderId::new(900_099_015);
        Order::limit_order(order_id, OrderSide::Bid, 99.into(), 15.into())
    }

    #[fixture]
    fn bid_020_at_016() -> Order {
        let order_id = OrderId::new(900_020_016);
        Order::limit_order(order_id, OrderSide::Bid, 20.into(), 16.into())
    }

    mod limit_orders {
        use super::*;

        #[rstest]
        fn insert_one_ask_one_ask_no_match(mut orderbook: Orderbook, ask_100_at_015: Order, bid_025_at_014: Order) {
            // different side not matching
            assert_ne!(ask_100_at_015.side(), bid_025_at_014.side());
            assert!(!bid_025_at_014.matches(&ask_100_at_015));

            assert_eq!(orderbook.handle_create(ask_100_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_025_at_014), NOT_MATCHED);

            // confirm the top for bid and the top for ask are the ones inserted
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), Some(&ask_100_at_015));
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), Some(&bid_025_at_014));
        }

        #[rstest]
        fn cancel_order(mut orderbook: Orderbook, ask_100_at_015: Order, bid_025_at_014: Order) {
            // different side not matching
            assert_ne!(ask_100_at_015.side(), bid_025_at_014.side());
            assert!(!bid_025_at_014.matches(&ask_100_at_015));

            assert_eq!(orderbook.handle_create(ask_100_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_025_at_014), NOT_MATCHED);

            // cancel the ask then confirm the top ask is empty and the top bid remains, finally try to cancel the same again
            assert_eq!(orderbook.handle_cancel(ask_100_at_015.id()).ok(), Some(ask_100_at_015));
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), None);
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), Some(&bid_025_at_014));
            assert_eq!(
                orderbook.handle_cancel(ask_100_at_015.id()),
                Err(OrderbookError::OrderToCancelNotFound(ask_100_at_015.id()))
            );
        }

        #[rstest]
        fn cancel_duplicated_order(mut orderbook: Orderbook, ask_100_at_015: Order) {
            assert_eq!(orderbook.handle_create(ask_100_at_015), NOT_MATCHED);
            assert_eq!(
                orderbook.handle_create(ask_100_at_015),
                Err(OrderbookError::OrderDuplicated(ask_100_at_015.id()))
            );
        }

        #[rstest]
        fn cancel_matched_order(mut orderbook: Orderbook, ask_100_at_015: Order, bid_099_at_015: Order) {
            // different side AND matching
            assert_ne!(ask_100_at_015.side(), bid_099_at_015.side());
            assert!(bid_099_at_015.matches(&ask_100_at_015));

            assert_eq!(orderbook.handle_create(bid_099_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(ask_100_at_015), MATCHED);

            // the bid in the limit book is gone because the incoming ask matched and completed the bid
            assert!(orderbook.peek_top(&OrderSide::Bid).is_none());
            assert!(orderbook.peek_top(&OrderSide::Ask).is_some());

            // there's a leftover ask hence the ask can be canceled
            assert_eq!(orderbook.handle_cancel(ask_100_at_015.id()).ok(), Some(ask_100_at_015));

            // the bid should be gone hence cannot be canceled!
            assert_eq!(
                orderbook.handle_cancel(bid_099_at_015.id()),
                Err(OrderbookError::OrderToCancelNotFound(bid_099_at_015.id()))
            );
        }

        #[rstest]
        fn insert_two_asks_same_price(mut orderbook: Orderbook, ask_100_at_015: Order, ask_080_at_015: Order) {
            // same side same price
            assert_eq!(ask_100_at_015.side(), ask_080_at_015.side());
            assert_eq!(ask_100_at_015.limit_price(), ask_080_at_015.limit_price());

            assert_eq!(orderbook.handle_create(ask_100_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(ask_080_at_015), NOT_MATCHED);

            // confirm the first ask is the one returned as top then cancel that one and confirm the other becomes the new top
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), Some(&ask_100_at_015));
            assert_eq!(orderbook.handle_cancel(ask_100_at_015.id()).ok(), Some(ask_100_at_015));
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), Some(&ask_080_at_015));
        }

        #[rstest]
        fn insert_two_asks_different_price(mut orderbook: Orderbook, ask_100_at_015: Order, ask_070_at_014: Order) {
            // same side different price (second order is a better ask)
            assert_eq!(ask_100_at_015.side(), ask_070_at_014.side());
            assert!(ask_100_at_015.limit_price().unwrap() > ask_070_at_014.limit_price().unwrap());

            // after next 2 lines we should have 2 ask levels with the second at the top
            assert_eq!(orderbook.handle_create(ask_100_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(ask_070_at_014), NOT_MATCHED);

            // confirm the second ask is the one returned as top then cancel that one and confirm the other becomes the new top
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), Some(&ask_070_at_014));
            assert_eq!(orderbook.handle_cancel(ask_070_at_014.id()).ok(), Some(ask_070_at_014));
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), Some(&ask_100_at_015));
        }

        #[rstest]
        fn insert_two_bids_different_price(mut orderbook: Orderbook, bid_099_at_015: Order, bid_020_at_016: Order) {
            // after next 2 lines we should have 2 bid levels with the second at the top
            assert_eq!(orderbook.handle_create(bid_099_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_020_at_016), NOT_MATCHED);

            // confirm the second bid is the one returned as top then cancel that one and confirm the other becomes the new top
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), Some(&bid_020_at_016));
            assert_eq!(orderbook.handle_cancel(bid_020_at_016.id()).ok(), Some(bid_020_at_016));
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), Some(&bid_099_at_015));
        }

        #[rstest]
        fn match_order_with_one_level(mut orderbook: Orderbook, ask_100_at_015: Order, bid_099_at_015: Order) {
            // different side AND matching
            assert_ne!(ask_100_at_015.side(), bid_099_at_015.side());
            assert!(bid_099_at_015.matches(&ask_100_at_015));

            let expected = ask_100_at_015.remaining() - bid_099_at_015.remaining();
            assert_eq!(orderbook.handle_create(bid_099_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(ask_100_at_015), MATCHED);

            // the ask is still open but the bid is gone
            match orderbook.peek_top(&OrderSide::Ask) {
                Some(top_ask) => {
                    assert_eq!(top_ask, &ask_100_at_015);
                    assert_eq!(top_ask.remaining(), expected);
                }
                None => panic!(),
            }
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), None);
        }

        #[rstest]
        fn match_order_with_two_levels(
            mut orderbook: Orderbook,
            ask_100_at_015: Order,
            bid_099_at_015: Order,
            bid_020_at_016: Order,
        ) {
            // different side AND matching
            assert_ne!(ask_100_at_015.side(), bid_099_at_015.side());
            assert_ne!(ask_100_at_015.side(), bid_020_at_016.side());
            assert!(bid_099_at_015.matches(&ask_100_at_015));
            assert!(bid_020_at_016.matches(&ask_100_at_015));

            let expected = bid_099_at_015.remaining() - (ask_100_at_015.remaining() - bid_020_at_016.remaining());
            assert_eq!(orderbook.handle_create(bid_099_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_020_at_016), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(ask_100_at_015), MATCHED);

            // the ask is gone and in the bid one is remaining
            match orderbook.peek_top(&OrderSide::Bid) {
                Some(top_bid) => {
                    assert_eq!(top_bid, &bid_099_at_015);
                    assert_eq!(top_bid.remaining(), expected);
                }
                None => panic!(),
            }
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), None);
        }
    }

    mod features {
        use crate::order::{OrderType, TimeInForce};

        use super::*;

        #[rstest]
        fn cancel_fill_or_kill(
            mut orderbook: Orderbook,
            ask_080_at_015: Order,
            ask_100_at_015: Order,
            bid_099_at_015: Order,
        ) {
            // keep the original limit price
            let limit_price = bid_099_at_015.limit_price().unwrap();

            // mutate incoming order to make it post only
            let mut bid_099_at_015 = bid_099_at_015;
            bid_099_at_015.mutate_type(OrderType::Limit {
                limit_price,
                time_in_force: TimeInForce::ImmediateOrCancel { fill_or_kill: true },
            });

            // confirm is FOK, it matches and is not closed
            assert!(bid_099_at_015.is_fill_or_kill());
            assert!(bid_099_at_015.matches(&ask_080_at_015));
            assert!(!bid_099_at_015.is_closed());

            // confirm it cannot be filled
            assert!(bid_099_at_015.remaining() > ask_080_at_015.remaining());

            // send to the book, there should be no matching
            assert_eq!(orderbook.handle_create(ask_080_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_099_at_015), NOT_MATCHED);

            // ask remains untouched in the top and there's no bid in the book
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), Some(&ask_080_at_015));
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), None);

            // now confirm that with another ask then the bid could be filled
            assert!(bid_099_at_015.remaining() < (ask_080_at_015.remaining() + ask_100_at_015.remaining()));
            assert!(bid_099_at_015.matches(&ask_100_at_015));

            // this is the amount that should be left in the asks
            let ask_remaining = ask_080_at_015.remaining() + ask_100_at_015.remaining() - bid_099_at_015.remaining();

            // send to the book, there should be matching
            assert_eq!(orderbook.handle_create(ask_100_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_099_at_015), MATCHED);

            // there's been a matching across two levels; there's no bid in the book
            assert_eq!(orderbook.peek_top(&OrderSide::Ask).unwrap().remaining(), ask_remaining);
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), None);
        }

        #[rstest]
        fn cancel_post_only(mut orderbook: Orderbook, ask_100_at_015: Order, bid_099_at_015: Order) {
            // keep the original limit price
            let limit_price = bid_099_at_015.limit_price().unwrap();

            // mutate incoming order to make it post only
            let mut bid_099_at_015 = bid_099_at_015;
            bid_099_at_015.mutate_type(OrderType::Limit {
                limit_price,
                time_in_force: TimeInForce::GoodTilCancel { post_only: true },
            });

            // confirm is post only, it matches and is not closed
            assert!(bid_099_at_015.is_post_only());
            assert!(bid_099_at_015.matches(&ask_100_at_015));
            assert!(!bid_099_at_015.is_closed());

            // send to the book, there should be no matching
            assert_eq!(orderbook.handle_create(ask_100_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_099_at_015), NOT_MATCHED);

            // ask remains untouched in the top and there's no bid in the book
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), Some(&ask_100_at_015));
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), None);
        }

        #[rstest]
        fn cancel_immediate_or_cancel(mut orderbook: Orderbook, ask_080_at_015: Order, bid_099_at_015: Order) {
            // keep the original limit price
            let limit_price = bid_099_at_015.limit_price().unwrap();

            // mutate incoming order to make it IOC
            let mut bid_099_at_015 = bid_099_at_015;
            bid_099_at_015.mutate_type(OrderType::Limit {
                limit_price,
                time_in_force: TimeInForce::ImmediateOrCancel { fill_or_kill: false },
            });

            // confirm is IOC, it matches and is not closed
            assert!(bid_099_at_015.is_immediate_or_cancel());
            assert!(bid_099_at_015.matches(&ask_080_at_015));
            assert!(!bid_099_at_015.is_closed());

            // send to the book, there should be matching
            assert_eq!(orderbook.handle_create(ask_080_at_015), NOT_MATCHED);
            assert_eq!(orderbook.handle_create(bid_099_at_015), MATCHED);

            // the ask is filled and the remaining bid doesn't remain in the book
            assert_eq!(orderbook.peek_top(&OrderSide::Ask), None);
            assert_eq!(orderbook.peek_top(&OrderSide::Bid), None);
        }
    }
}
