use alloy_primitives::IntoLogData;
use primitives::{Address, Log};

use crate::{host::PerpHost, interface::IPerpDex, types::PerpPosition, PERP_DEX_ADDRESS};

pub(crate) fn position_changed_log(
    user: Address,
    market_id: u64,
    position: &PerpPosition,
    realized_pnl: i64,
    closed_quantity: u64,
) -> Log {
    Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user,
            marketId: market_id,
            amount: position.amount,
            vQuoteBalance: position.v_quote_balance,
            margin: position.margin,
            marginReserved: position.margin_reserved,
            feeReserved: position.fee_reserved,
            leverage: position.leverage,
            realizedPnl: realized_pnl,
            closedQuantity: closed_quantity,
        }
        .to_log_data(),
    }
}

pub(crate) fn emit_position_changed<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    position: &PerpPosition,
    realized_pnl: i64,
    closed_quantity: u64,
) {
    context.log(position_changed_log(
        user,
        market_id,
        position,
        realized_pnl,
        closed_quantity,
    ));
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolEvent;
    use primitives::address;

    use super::*;

    #[test]
    fn position_changed_log_contains_complete_position_state() {
        let user = address!("1111111111111111111111111111111111111111");
        let position = PerpPosition {
            amount: -25,
            v_quote_balance: 2_250,
            margin: 500,
            margin_reserved: 70,
            fee_reserved: 3,
            leverage: 4,
            ..PerpPosition::default()
        };

        let log = position_changed_log(user, 7, &position, -11, 9);
        let decoded = IPerpDex::PositionChanged::decode_raw_log(log.data.topics(), &log.data.data)
            .unwrap_or_else(|error| panic!("position log should decode: {error}"));

        assert_eq!(decoded.user, user);
        assert_eq!(decoded.marketId, 7);
        assert_eq!(decoded.amount, -25);
        assert_eq!(decoded.vQuoteBalance, 2_250);
        assert_eq!(decoded.margin, 500);
        assert_eq!(decoded.marginReserved, 70);
        assert_eq!(decoded.feeReserved, 3);
        assert_eq!(decoded.leverage, 4);
        assert_eq!(decoded.realizedPnl, -11);
        assert_eq!(decoded.closedQuantity, 9);
    }
}
