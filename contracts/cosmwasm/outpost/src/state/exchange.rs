use cosmwasm_std::{Deps, DepsMut};
use cvm_route::exchange::ExchangeItem;
use cvm_runtime::exchange::ExchangeId;
use cw_storage_plus::Map;

use crate::{
    auth,
    batch::BatchResponse,
    error::{ContractError, Result},
    events::make_event,
};

pub(crate) fn get_by_id(deps: Deps, exchange_id: ExchangeId) -> Result<ExchangeItem> {
    EXCHANGE
        .may_load(deps.storage, exchange_id.0)?
        .ok_or(ContractError::ExchangeNotFound)
}

pub(crate) fn force_exchange(
    _: auth::Admin,
    deps: DepsMut,
    msg: ExchangeItem,
) -> Result<BatchResponse> {
    EXCHANGE.save(deps.storage, msg.exchange_id.0, &msg)?;
    Ok(BatchResponse::new().add_event(
        make_event("exchange.forced").add_attribute("exchange_od", msg.exchange_id.to_string()),
    ))
}

pub(crate) const EXCHANGE: Map<u128, cvm_route::exchange::ExchangeItem> = Map::new("exchange");
