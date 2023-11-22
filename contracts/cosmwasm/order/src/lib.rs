#![allow(clippy::disallowed_methods)] // does unwrap inside
#![allow(deprecated)] // sylvia macro

mod prelude;
mod store;
mod types;

use prelude::*;
use store::*;
pub use types::*;

pub use crate::sv::{ExecMsg, QueryMsg};
use cosmwasm_schema::{cw_serde, schemars};
use cosmwasm_std::{
    wasm_execute, Addr, BankMsg, Coin, Event, Order, StdError, Storage, Uint128, Uint64,
};
use cvm::{
    instruction::ExchangeId,
    shared::{XcInstruction, XcProgram},
};
use cw_storage_plus::{Index, IndexList, IndexedMap, Item, Map, MultiIndex};
use sylvia::{
    contract,
    cw_std::{ensure, Response, StdResult},
    entry_points,
    types::{ExecCtx, InstantiateCtx, QueryCtx},
};

/// so this is just to make code easy to read, we will optimize later
use num_rational::BigRational;

use cvm::network::NetworkId;

pub struct OrderContract<'a> {
    pub orders: Map<'a, u128, OrderItem>,
    /// (a,b,solver)
    pub solutions:
        IndexedMap<'a, &'a (Denom, Denom, SolverAddress), SolutionItem, SolutionIndexes<'a>>,
    pub next_order_id: Item<'a, u128>,
    /// address for CVM contact to send routes to
    pub cvm_address: Item<'a, String>,
    pub admin: cw_controllers::Admin<'a>,
}

impl Default for OrderContract<'_> {
    fn default() -> Self {
        Self {
            orders: Map::new("orders"),
            next_order_id: Item::new("next_order_id"),
            cvm_address: Item::new("cvm_address"),
            solutions: solutions(),
            admin: cw_controllers::Admin::new("admin"),
        }
    }
}

#[entry_points]
#[contract]
impl OrderContract<'_> {
    pub fn new() -> Self {
        Self::default()
    }
    #[msg(instantiate)]
    pub fn instantiate(
        &self,
        mut ctx: InstantiateCtx,
        admin: Option<Addr>,
        cvm_address: Addr,
    ) -> StdResult<Response> {
        self.cvm_address
            .save(ctx.deps.storage, &cvm_address.into_string())?;
        self.admin
            .set(ctx.deps.branch(), Some(admin.unwrap_or(ctx.info.sender)))?;
        Ok(Response::default())
    }

    /// This contracts receives user order, takes ddos protection deposit (to protect solvers from
    /// spamming), and stores order for searchers.
    #[msg(exec)]
    pub fn order(&self, ctx: ExecCtx, msg: OrderSubMsg) -> StdResult<Response> {
        // for now we just use bank for ics20 tokens
        let funds = ctx
            .info
            .funds
            .get(0)
            .expect("there are some funds in order");

        // just save order under incremented id
        let order_id = self
            .next_order_id
            .load(ctx.deps.storage)
            .unwrap_or_default();
        let order = OrderItem {
            msg,
            given: funds.clone(),
            order_id: order_id.into(),
            owner: ctx.info.sender,
        };
        self.orders.save(ctx.deps.storage, order_id, &order)?;
        self.next_order_id.save(ctx.deps.storage, &(order_id + 1))?;
        let order_created = order_created(order_id, &order);
        ctx.deps
            .api
            .debug(&format!("mantis::order::created: {:?}", order));
        Ok(Response::default().add_event(order_created))
    }

    /// Hook/crank for cleanup.
    /// Caller receives small reward for doing so.
    /// This is to prevent spamming of old orders.
    /// If input collections are empty, one clean ups ALL orders
    #[msg(exec)]
    pub fn timeout(
        &self,
        ctx: ExecCtx,
        orders: Vec<OrderId>,
        solutions: Vec<Addr>,
    ) -> StdResult<Response> {
        let orders: Result<Vec<(u128, OrderItem)>, _> = self
            .orders
            .range(ctx.deps.storage, None, None, Order::Ascending)
            .filter(|x| {
                let (_id, order) = x.as_ref().unwrap();
                order.msg.timeout < ctx.env.block.height
            })
            .collect();
        let mut msgs = vec![];
        for order in orders? {
            self.orders.remove(ctx.deps.storage, order.0);
            let msg = BankMsg::Send {
                amount: [order.1.given].to_vec(),
                to_address: order.1.owner.to_string(),
            };
            msgs.push(msg);
        }
        Ok(Response::default().add_messages(msgs))
    }

    /// Allow admin to move stuck funds.
    #[msg(exec)]
    pub fn force_transfer(
        &self,
        ctx: ExecCtx,
        amount: Vec<Coin>,
        to_address: String,
    ) -> StdResult<Response> {
        ensure!(
            self.admin.is_admin(ctx.deps.as_ref(), &ctx.info.sender)?,
            StdError::generic_err("only admin can call this")
        );
        let msg: BankMsg = BankMsg::Send { to_address, amount };
        Ok(Response::default().add_message(msg))
    }

    /// until order/solution in execution can cancel
    /// cancellation of order is delayed so solvers can observe it
    /// can remove up only my orders and solution
    #[msg(exec)]
    pub fn cancel(
        &self,
        _ctx: ExecCtx,
        _orders: Vec<OrderId>,
        _solution: Option<Addr>,
    ) -> StdResult<Response> {
        todo!("remove order and send event")
    }

    #[msg(exec)]
    pub fn route(&self, ctx: ExecCtx, msg: RouteSubMsg) -> StdResult<Response> {
        ensure!(
            ctx.info.sender == ctx.env.contract.address,
            StdError::GenericErr {
                msg: "only self can call this".to_string()
            }
        );

        ctx.deps.api.debug(
            "so here we add route execution tracking to storage and map route to CVM program",
        );

        let cvm = Self::traverse_route(msg.route);
        let cvm = cvm::gateway::ExecuteMsg::ExecuteProgram(cvm::gateway::ExecuteProgramMsg {
            salt: vec![],
            program: cvm,
            assets: <_>::default(),
            tip: None,
        });
        let contract = self.cvm_address.load(ctx.deps.storage)?;
        let cvm = wasm_execute(contract, &cvm, vec![])?;
        Ok(Response::default().add_message(cvm))
    }

    /// converts high level route to CVM program
    fn traverse_route(route: ExchangeRoute) -> cvm::shared::XcProgram {
        let mut program = XcProgram {
            tag: b"may be use solution id and some chain for tracking".to_vec(),
            instructions: vec![],
        };

        let mut exchanges = Self::traverse_exchanges(route.exchanges);
        program.instructions.append(&mut exchanges);

        let mut spawns = Self::traverse_spawns(route.spawns);
        program.instructions.append(&mut spawns);

        program
    }

    fn traverse_spawns(spawns: Vec<Spawn<ExchangeRoute>>) -> Vec<cvm::shared::XcInstruction> {
        let mut result = vec![];
        for spawn in spawns {
            let spawn = if let Some(execute) = spawn.execute {
                let program = Self::traverse_route(execute);
                XcInstruction::Spawn {
                    network_id: spawn.to_chain,
                    salt: b"solution".to_vec(),
                    assets: <_>::default(), // map spawn.carry to CVM assets
                    program,
                }
            } else {
                XcInstruction::Spawn {
                    network_id: spawn.to_chain,
                    salt: b"solution".to_vec(),
                    assets: <_>::default(), // map spawn.carry to CVM assets
                    program: XcProgram {
                        tag: b"solution".to_vec(),
                        instructions: vec![], // we really just do final transfer
                    },
                }
            };
            result.push(spawn);
        }
        result
    }

    fn traverse_exchanges(_exchanges: Vec<Exchange>) -> Vec<cvm::shared::XcInstruction> {
        // here map each exchange to CVM instruction
        // for each pool get its denom, and do swaps
        vec![]
    }

    /// Provides solution for set of orders.
    /// All fully
    #[msg(exec)]
    pub fn solve(&self, ctx: ExecCtx, msg: SolutionSubMsg) -> StdResult<Response> {
        // read all orders as solver provided
        let mut all_orders = self.merge_solution_with_orders(&msg, &ctx)?;
        let at_least_one = all_orders.first().expect("at least one");

        // normalize pair
        let mut ab = (
            at_least_one.given().denom.clone(),
            at_least_one.wants().denom.clone(),
        );
        ab.sort_selection();

        // add solution to total solutions
        let possible_solution = SolutionItem {
            pair: ab.clone(),
            msg,
            block_added: ctx.env.block.height,
        };

        self.solutions.save(
            ctx.deps.storage,
            &(
                ab.clone().0,
                ab.clone().1,
                ctx.info.sender.clone().to_string(),
            ),
            &possible_solution,
        )?;
        let solution_upserted = Event::new("mantis-solution-upserted")
            .add_attribute("token_a", ab.clone().0)
            .add_attribute("token_b", ab.clone().1)
            .add_attribute("solver_address", &ctx.info.sender.to_string());
        ctx.deps.api.debug(&format!(
            "mantis::solution::upserted {:?}",
            &solution_upserted
        ));

        // get all solution for pair
        let all_solutions: Result<Vec<SolutionItem>, _> = self
            .solutions
            .prefix(ab.clone())
            .range(ctx.deps.storage, None, None, Order::Ascending)
            .map(|r| r.map(|(_, solution)| solution))
            .collect();
        let all_solutions = all_solutions?;
        ctx.deps
            .api
            .debug(&format!("mantis::solutions::current {:?}", all_solutions));

        // pick up optimal solution with solves with bank
        let mut a_in = 0;
        let mut b_in = 0;
        let (a, b) = ab.clone();
        let mut transfers = vec![];
        let mut solution_item: SolutionItem = possible_solution;
        for solution in all_solutions {
            let alternative_all_orders = self.merge_solution_with_orders(&solution.msg, &ctx)?;
            let a_total_in: u128 = alternative_all_orders
                .iter()
                .filter(|x| x.given().denom == a)
                .map(|x: &SolvedOrder| x.given().amount.u128())
                .sum();
            let b_total_in: u128 = alternative_all_orders
                .iter()
                .filter(|x: &&SolvedOrder| x.given().denom == b)
                .map(|x| x.given().amount.u128())
                .sum();

            let alternative_transfers = solves_cows_via_bank(
                &alternative_all_orders.clone(),
                a.clone(),
                a_total_in,
                b_total_in,
            );
            ctx.deps.api.debug(&format!(
                "mantis::solutions::alternative {:?}",
                &alternative_transfers
            ));
            if let Ok(alternative_transfers) = alternative_transfers {
                if a_total_in * b_total_in > a_in * b_in {
                    a_in = a_total_in;
                    b_in = b_total_in;
                    all_orders = alternative_all_orders;
                    transfers = alternative_transfers;
                    solution_item = solution;
                }
            }
        }

        let mut response = Response::default();

        if let Some(route) = solution_item.msg.route {
            // send remaining for settlement
            let route = wasm_execute(
                ctx.env.contract.address,
                &ExecMsg::route(RouteSubMsg { all_orders, route }),
                vec![],
            )?;
            response = response.add_message(route);
        };

        let solution_chosen = Event::new("mantis-solution-chosen")
            .add_attribute("token_a", ab.clone().0)
            .add_attribute("token_b", ab.clone().1)
            .add_attribute("solver_address", &ctx.info.sender.to_string());
        let transfers = self.fill(
            ctx.deps.storage,
            transfers,
            ctx.info.sender.to_string(),
            solution_item.block_added,
        )?;
        ctx.deps
            .api
            .debug(&format!("mantis-solution-chosen: {:?}", &solution_chosen));
        for transfer in transfers {
            response = response.add_message(transfer.bank_msg);
            response = response.add_event(transfer.event);
        }
        Ok(response
            .add_event(solution_upserted)
            .add_event(solution_chosen))
    }

    fn merge_solution_with_orders(
        &self,
        msg: &SolutionSubMsg,
        ctx: &ExecCtx<'_>,
    ) -> Result<Vec<SolvedOrder>, StdError> {
        let all_orders = msg
            .cows
            .iter()
            .map(|x| {
                self.orders
                    .load(ctx.deps.storage, x.order_id.u128())
                    .map_err(|_| StdError::not_found("order"))
                    .and_then(|order| SolvedOrder::new(order, x.clone()))
            })
            .collect::<Result<Vec<SolvedOrder>, _>>()?;
        Ok(all_orders)
    }

    /// Simple get all orders
    #[msg(query)]
    pub fn get_all_orders(&self, ctx: QueryCtx) -> StdResult<Vec<OrderItem>> {
        self.orders
            .range_raw(ctx.deps.storage, None, None, Order::Ascending)
            .map(|r| r.map(|(_, order)| order))
            .collect::<StdResult<Vec<OrderItem>>>()
    }

    #[msg(query)]
    pub fn get_all_solutions(&self, ctx: QueryCtx) -> StdResult<Vec<SolutionItem>> {
        self.get_solutions(ctx.deps.storage)
    }

    fn get_solutions(&self, storage: &dyn Storage) -> Result<Vec<SolutionItem>, StdError> {
        self.solutions
            .idx
            .pair
            .range(storage, None, None, Order::Ascending)
            .map(|r| r.map(|(_, x)| x))
            .collect()
    }

    /// (partially) fills orders.
    /// Returns relevant transfers and sets proper tracking info for remaining cross chain
    /// execution. Orders which are in cross chain execution are "locked", users cannot cancel them
    /// or take funds back during execution (because funds are moved).
    fn fill(
        &self,
        storage: &mut dyn Storage,
        cows: Vec<CowFilledOrder>,
        solver_address: String,
        solution_block_added: u64,
    ) -> StdResult<Vec<CowFillResult>> {
        let mut results = vec![];
        for (transfer, order) in cows.into_iter() {
            let mut order: OrderItem = self.orders.load(storage, order.u128())?;
            order.fill(transfer.amount);
            let event = if order.given.amount.is_zero() {
                self.orders.remove(storage, order.order_id.u128());
                Event::new("mantis-order-filled-full")
                    .add_attribute("order_id", order.order_id.to_string())
                    .add_attribute("solver_address", solver_address.clone())
                    .add_attribute("solution_block_added", solution_block_added.to_string())
            } else {
                self.orders.save(storage, order.order_id.u128(), &order)?;
                Event::new("mantis-order-filled-parts")
                    .add_attribute("order_id", order.order_id.to_string())
                    .add_attribute("amount", transfer.amount.to_string())
                    .add_attribute("solver_address", solver_address.clone())
                    .add_attribute("solution_block_added", solution_block_added.to_string())
            };
            let transfer = BankMsg::Send {
                to_address: order.owner.to_string(),
                amount: vec![transfer],
            };
            results.push(CowFillResult {
                bank_msg: transfer,
                event,
            });
        }
        Ok(results)
    }
}

fn order_created(order_id: u128, order: &OrderItem) -> Event {
    let order_created = Event::new("mantis-order-created")
        .add_attribute("order_id", order_id.to_string())
        .add_attribute("order_given_amount", order.given.amount.to_string())
        .add_attribute("order_given_denom", order.given.denom.to_string())
        .add_attribute("order_owner", order.owner.to_string())
        .add_attribute("order_wants_amount", order.msg.wants.amount.to_string())
        .add_attribute("order_wants_denom", order.msg.wants.denom.to_string());
    order_created
}

/// given all orders amounts aggregated into common pool,
/// ensure that solution does not violates this pull
/// and return proper action to handle settling funds locally according solution
fn solves_cows_via_bank(
    all_orders: &Vec<SolvedOrder>,
    a: String,
    a_total_in: u128,
    b_total_in: u128,
) -> Result<Vec<CowFilledOrder>, StdError> {
    let mut a_total_in = BigRational::from_integer(a_total_in.into());
    let mut b_total_in = BigRational::from_integer(b_total_in.into());
    let mut transfers = vec![];
    for order in all_orders.iter() {
        let cowed = order.solution.cow_amount;
        let amount = Coin {
            amount: cowed,
            ..order.given().clone()
        };

        // so if not enough was deposited as was taken from original orders, it will fails - so
        // solver cannot rob the bank
        if amount.denom == a {
            a_total_in -= BigRational::from_integer(cowed.u128().into());
        } else {
            b_total_in -= BigRational::from_integer(cowed.u128().into());
        };

        transfers.push((amount, order.order.order_id));
    }
    if a_total_in < BigRational::default() || b_total_in < BigRational::default() {
        return Err(StdError::generic_err("SolutionForCowsViaBankIsNotBalanced"));
    }
    Ok(transfers)
}
