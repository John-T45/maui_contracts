use cosmwasm_std::{
    log, Api, BankMsg, Coin, CosmosMsg, Decimal, Env, Extern, HandleResponse, HandleResult,
    HumanAddr, Querier, StdError, StdResult, Storage, Uint128,
};

use crate::math::{decimal_division, decimal_multiplication, decimal_subtraction};
use crate::msg::{LiabilitiesResponse, LiabilityResponse, LoanAmountResponse};
use crate::state::{
    read_config, read_liabilities, read_liability, read_state, store_liability, store_state,
    Config, Liability, State,
};

use moneymarket::{
    deduct_tax, query_borrow_limit, query_borrow_rate, BorrowLimitResponse, BorrowRateResponse,
};
use terraswap::query_balance;

pub fn borrow_stable<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    borrow_amount: Uint128,
    to: Option<HumanAddr>,
) -> HandleResult {
    let config: Config = read_config(&deps.storage)?;

    // Update interest related state
    let mut state: State = read_state(&deps.storage)?;
    compute_interest(&deps, &config, &mut state, env.block.height)?;

    let borrower = env.message.sender;
    let borrower_raw = deps.api.canonical_address(&borrower)?;
    let mut liability: Liability = read_liability(&deps.storage, &borrower_raw);
    compute_loan(&state, &mut liability);

    let overseer = deps.api.human_address(&config.overseer_contract)?;
    let borrow_limit_res: BorrowLimitResponse = query_borrow_limit(deps, &overseer, &borrower)?;

    if borrow_limit_res.borrow_limit < borrow_amount + liability.loan_amount {
        return Err(StdError::generic_err("Cannot borrow more than limit"));
    }

    liability.loan_amount += borrow_amount;
    state.total_liabilities = state.total_liabilities + Decimal::from_ratio(borrow_amount, 1u128);
    store_state(&mut deps.storage, &state)?;
    store_liability(&mut deps.storage, &borrower_raw, &liability)?;

    Ok(HandleResponse {
        messages: vec![CosmosMsg::Bank(BankMsg::Send {
            from_address: env.contract.address,
            to_address: to.unwrap_or_else(|| borrower.clone()),
            amount: vec![deduct_tax(
                &deps,
                Coin {
                    denom: config.stable_denom,
                    amount: borrow_amount,
                },
            )?],
        })],
        log: vec![
            log("action", "borrow_stable"),
            log("borrower", borrower),
            log("borrow_amount", borrow_amount),
        ],
        data: None,
    })
}

pub fn repay_stable_from_liquidation<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    borrower: HumanAddr,
    prev_balance: Uint128,
) -> HandleResult {
    let config: Config = read_config(&deps.storage)?;
    if config.overseer_contract != deps.api.canonical_address(&env.message.sender)? {
        return Err(StdError::unauthorized());
    }

    let cur_balance: Uint128 = query_balance(
        &deps,
        &env.contract.address,
        config.stable_denom.to_string(),
    )?;

    // override env
    let mut env = env;

    env.message.sender = borrower;
    env.message.sent_funds = vec![Coin {
        denom: config.stable_denom,
        amount: (cur_balance - prev_balance).unwrap(),
    }];

    repay_stable(deps, env)
}

pub fn repay_stable<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
) -> HandleResult {
    let config: Config = read_config(&deps.storage)?;

    // Check base denom deposit
    let amount: Uint128 = env
        .message
        .sent_funds
        .iter()
        .find(|c| c.denom == config.stable_denom)
        .map(|c| c.amount)
        .unwrap_or_else(Uint128::zero);

    // Cannot deposit zero amount
    if amount.is_zero() {
        return Err(StdError::generic_err("Cannot repay zero coins"));
    }

    // Update interest related state
    let mut state: State = read_state(&deps.storage)?;
    compute_interest(&deps, &config, &mut state, env.block.height)?;

    let borrower = env.message.sender;
    let borrower_raw = deps.api.canonical_address(&borrower)?;
    let mut liability: Liability = read_liability(&deps.storage, &borrower_raw);
    compute_loan(&state, &mut liability);

    let repay_amount: Uint128;
    let mut messages: Vec<CosmosMsg> = vec![];
    if liability.loan_amount < amount {
        repay_amount = liability.loan_amount;
        liability.loan_amount = Uint128::zero();

        // Payback left repay amount to sender
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            from_address: env.contract.address,
            to_address: borrower.clone(),
            amount: vec![deduct_tax(
                &deps,
                Coin {
                    denom: config.stable_denom,
                    amount: (amount - repay_amount)?,
                },
            )?],
        }));
    } else {
        repay_amount = amount;
        liability.loan_amount = (liability.loan_amount - repay_amount)?;
    }

    state.total_liabilities = decimal_subtraction(
        state.total_liabilities,
        Decimal::from_ratio(repay_amount, 1u128),
    );

    store_liability(&mut deps.storage, &borrower_raw, &liability)?;
    store_state(&mut deps.storage, &state)?;

    Ok(HandleResponse {
        messages,
        log: vec![
            log("action", "repay_stable"),
            log("borrower", borrower),
            log("repay_amount", repay_amount),
        ],
        data: None,
    })
}

/// Compute interest and update state
/// total liabilities and total reserves
pub fn compute_interest<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    config: &Config,
    state: &mut State,
    block_height: u64,
) -> StdResult<()> {
    let borrow_rate_res: BorrowRateResponse =
        query_borrow_rate(&deps, &deps.api.human_address(&config.interest_model)?)?;

    let passed_blocks = block_height - state.last_interest_updated;
    let interest_factor: Decimal = decimal_multiplication(
        Decimal::from_ratio(passed_blocks, 1u128),
        borrow_rate_res.rate,
    );

    let interest_accrued = decimal_multiplication(state.total_liabilities, interest_factor);

    state.global_interest_index = decimal_multiplication(
        state.global_interest_index,
        Decimal::one() + interest_factor,
    );
    state.total_liabilities = state.total_liabilities + interest_accrued;
    state.total_reserves =
        state.total_reserves + decimal_multiplication(interest_accrued, config.reserve_factor);
    state.last_interest_updated = block_height;

    Ok(())
}

/// Compute new interest and apply to liability
fn compute_loan(state: &State, liability: &mut Liability) {
    liability.loan_amount = liability.loan_amount
        * decimal_division(state.global_interest_index, liability.interest_index);
    liability.interest_index = state.global_interest_index;
}

pub fn query_liability<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    borrower: HumanAddr,
) -> StdResult<LiabilityResponse> {
    let liability: Liability =
        read_liability(&deps.storage, &deps.api.canonical_address(&borrower)?);

    Ok(LiabilityResponse {
        borrower,
        interest_index: liability.interest_index,
        loan_amount: liability.loan_amount,
    })
}

pub fn query_liabilities<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    start_after: Option<HumanAddr>,
    limit: Option<u32>,
) -> StdResult<LiabilitiesResponse> {
    let start_after = if let Some(start_after) = start_after {
        Some(deps.api.canonical_address(&start_after)?)
    } else {
        None
    };

    let liabilities: Vec<LiabilityResponse> = read_liabilities(&deps, start_after, limit)?;
    Ok(LiabilitiesResponse { liabilities })
}

pub fn query_loan_amount<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    borrower: HumanAddr,
    block_height: u64,
) -> StdResult<LoanAmountResponse> {
    let config: Config = read_config(&deps.storage)?;
    let mut state: State = read_state(&deps.storage)?;
    let mut liability: Liability =
        read_liability(&deps.storage, &deps.api.canonical_address(&borrower)?);

    compute_interest(&deps, &config, &mut state, block_height)?;
    compute_loan(&state, &mut liability);

    Ok(LoanAmountResponse {
        borrower,
        loan_amount: liability.loan_amount,
    })
}
