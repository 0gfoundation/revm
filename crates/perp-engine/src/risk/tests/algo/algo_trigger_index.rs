use super::*;

fn id(n: u8) -> [u8; 32] {
    [n; 32]
}

#[test]
fn up_prices_are_ascending_and_down_prices_descending() {
    let mut ctx = make_ctx();
    for (p, n) in [(300u64, 1u8), (100, 2), (200, 3)] {
        storage::algo_index_insert(&mut ctx, MARKET_ID, true, p, id(n)).unwrap();
        storage::algo_index_insert(&mut ctx, MARKET_ID, false, p, id(n)).unwrap();
    }
    assert_eq!(
        storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![100, 200, 300],
        "up list ASC — fires on mark >= trigger, so the LOWEST trigger fires first"
    );
    assert_eq!(
        storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![300, 200, 100],
        "down list DESC — fires on mark <= trigger, so the HIGHEST trigger fires first"
    );
}

#[test]
fn a_price_leaves_the_sorted_list_only_when_its_level_empties() {
    let mut ctx = make_ctx();
    storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(1)).unwrap();
    storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(2)).unwrap();
    assert_eq!(storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap(), vec![100]);

    storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
    assert_eq!(
        storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![100],
        "one order left at this price — the price must stay, or the sweep would skip it"
    );
    assert_eq!(storage::load_algo_level(&mut ctx, MARKET_ID, true, 100).unwrap(), vec![id(2)]);

    storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(2)).unwrap();
    assert!(
        storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
        "level empty — the price must go, or the sweep walks a price with nothing behind it"
    );
}

#[test]
fn removing_an_unindexed_order_is_a_no_op() {
    let mut ctx = make_ctx();
    storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(1)).unwrap();
    // Idempotent on purpose: cancel, liquidation cancel-all and the sweep can all reach the
    // same order, and the second remover must not corrupt the price list.
    storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(9)).unwrap();
    storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
    storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
    assert!(storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
}

#[test]
fn the_two_directions_are_independent_at_the_same_price() {
    let mut ctx = make_ctx();
    storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(1)).unwrap();
    storage::algo_index_insert(&mut ctx, MARKET_ID, false, 100, id(2)).unwrap();
    storage::algo_index_remove(&mut ctx, MARKET_ID, true, 100, &id(1)).unwrap();
    assert!(storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    assert_eq!(
        storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![100],
        "a STOP and a TAKE_PROFIT can share a trigger price and must not evict each other"
    );
}

#[test]
fn orders_at_one_trigger_price_keep_placement_order() {
    let mut ctx = make_ctx();
    for n in [5u8, 3, 9] {
        storage::algo_index_insert(&mut ctx, MARKET_ID, true, 100, id(n)).unwrap();
    }
    assert_eq!(
        storage::load_algo_level(&mut ctx, MARKET_ID, true, 100).unwrap(),
        vec![id(5), id(3), id(9)],
        "FIFO by placement, mirroring the book's level queue"
    );
}
