use super::marginfi_group::{Bank, LendingPool, MarginfiGroup, WrappedI80F48};
use crate::{
    check, math_error,
    prelude::{MarginfiError, MarginfiResult},
};
use anchor_lang::prelude::*;
use anchor_spl::token::{transfer, Transfer};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use pyth_sdk_solana::{Price, PriceFeed};
use std::{
    cmp::{max, min},
    collections::HashMap,
};

#[account(zero_copy)]
pub struct MarginfiAccount {
    pub group: Pubkey,
    pub owner: Pubkey,
    pub lending_account: LendingAccount,
}

impl MarginfiAccount {
    /// Set the initial data for the marginfi account.
    pub fn initialize(&mut self, group: Pubkey, owner: Pubkey) {
        self.owner = owner;
        self.group = group;
    }
}

const EXP_10_I80F48: [I80F48; 15] = [
    I80F48!(1),
    I80F48!(10),
    I80F48!(100),
    I80F48!(1_000),
    I80F48!(10_000),
    I80F48!(100_000),
    I80F48!(1_000_000),
    I80F48!(10_000_000),
    I80F48!(100_000_000),
    I80F48!(1_000_000_000),
    I80F48!(10_000_000_000),
    I80F48!(100_000_000_000),
    I80F48!(1_000_000_000_000),
    I80F48!(10_000_000_000_000),
    I80F48!(100_000_000_000_000),
];

const EXPONENT: i32 = 6;

/// Convert a price `price.price` with decimal exponent `price.expo` to an I80F48 representation with exponent 6.
pub fn pyth_price_to_i80f48(price: &Price) -> MarginfiResult<I80F48> {
    let pyth_price = price.price;
    let pyth_expo = price.expo;

    let expo_delta = EXPONENT - pyth_expo;
    let expo_scale = EXP_10_I80F48[expo_delta.unsigned_abs() as usize];

    let price = I80F48::from_num(pyth_price);

    let price = if expo_delta < 0 {
        price.checked_div(expo_scale).ok_or_else(math_error!())?
    } else {
        price.checked_mul(expo_scale).ok_or_else(math_error!())?
    };

    Ok(price)
}

pub enum WeightType {
    Initial,
    Maintenance,
}

pub struct BankAccountWithPriceFeed<'a> {
    bank: &'a Bank,
    price_feed: PriceFeed,
    balance: &'a Balance,
}

impl<'a> BankAccountWithPriceFeed<'a> {
    pub fn load<'b: 'a, 'info: 'a + 'b>(
        lending_account: &'a LendingAccount,
        lending_pool: &'a LendingPool,
        pyth_accounts: &'b [AccountInfo<'info>],
    ) -> MarginfiResult<Vec<BankAccountWithPriceFeed<'a>>> {
        let pyth_accounts = create_pyth_account_map(pyth_accounts)?;

        lending_account
            .balances
            .iter()
            .filter_map(|b| b.as_ref())
            .map(|balance| {
                let bank = lending_pool
                    .banks
                    .get(balance.bank_index as usize)
                    .unwrap()
                    .as_ref()
                    .unwrap();

                let price_feed = bank.load_price_feed(&pyth_accounts)?;

                Ok(BankAccountWithPriceFeed {
                    bank,
                    price_feed,
                    balance,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn calc_weighted_assets_and_liabilities_values(
        &self,
        weight_type: WeightType,
    ) -> MarginfiResult<(I80F48, I80F48)> {
        // TODO: Expire price, and check confidence interval
        let price = self.price_feed.get_price_unchecked();

        let deposits_qt = self
            .bank
            .get_deposit_amount(self.balance.deposit_shares.into())?;
        let liabilities_qt = self
            .bank
            .get_deposit_amount(self.balance.liability_shares.into())?;
        let (deposit_weight, liability_weight) = self.bank.config.get_weights(weight_type); // TODO: asset-specific weights

        Ok((
            calc_asset_value(deposits_qt, &price, Some(deposit_weight))?,
            calc_asset_value(liabilities_qt, &price, Some(liability_weight))?,
        ))
    }
}

pub fn create_pyth_account_map<'a, 'info>(
    pyth_accounts: &'a [AccountInfo<'info>],
) -> MarginfiResult<HashMap<Pubkey, &'a AccountInfo<'info>>> {
    Ok(HashMap::from_iter(
        pyth_accounts.iter().map(|a| (a.key(), a)),
    ))
}

#[inline]
pub fn calc_asset_value(
    asset_quantity: I80F48,
    pyth_price: &Price,
    weight: Option<I80F48>,
) -> MarginfiResult<I80F48> {
    let price = pyth_price_to_i80f48(pyth_price)?;
    let scaling_factor = EXP_10_I80F48[pyth_price.expo.unsigned_abs() as usize];

    let weighted_asset_qt = if let Some(weight) = weight {
        asset_quantity.checked_mul(weight).unwrap()
    } else {
        asset_quantity
    };

    let asset_value = weighted_asset_qt
        .checked_mul(price)
        .ok_or_else(math_error!())?
        .checked_div(scaling_factor)
        .ok_or_else(math_error!())?;

    Ok(asset_value)
}

#[inline]
pub fn calc_asset_quantity(asset_value: I80F48, pyth_price: &Price) -> MarginfiResult<I80F48> {
    let price = pyth_price_to_i80f48(pyth_price)?;
    let scaling_factor = EXP_10_I80F48[pyth_price.expo.unsigned_abs() as usize];

    let asset_qt = asset_value
        .checked_mul(scaling_factor)
        .ok_or_else(math_error!())?
        .checked_div(price)
        .ok_or_else(math_error!())?;

    Ok(asset_qt)
}

pub enum RiskRequirementType {
    Initial,
    Maintenance,
}

impl RiskRequirementType {
    pub fn to_weight_type(&self) -> WeightType {
        match self {
            RiskRequirementType::Initial => WeightType::Initial,
            RiskRequirementType::Maintenance => WeightType::Maintenance,
        }
    }
}

pub struct RiskEngine<'a> {
    bank_accounts_with_price: Vec<BankAccountWithPriceFeed<'a>>,
}

impl<'a> RiskEngine<'a> {
    pub fn new<'b: 'a, 'info: 'a + 'b>(
        margin_group: &'a MarginfiGroup,
        marginfi_account: &'a MarginfiAccount,
        oracle_ais: &'b [AccountInfo<'info>],
    ) -> MarginfiResult<Self> {
        let bank_accounts_with_price = BankAccountWithPriceFeed::load(
            &marginfi_account.lending_account,
            &margin_group.lending_pool,
            oracle_ais,
        )?;

        Ok(Self {
            bank_accounts_with_price,
        })
    }

    pub fn check_account_health(&self, requirement_type: RiskRequirementType) -> MarginfiResult {
        let (total_weighted_assets, total_weighted_liabilities) = self
            .bank_accounts_with_price
            .iter()
            .map(|a| {
                a.calc_weighted_assets_and_liabilities_values(requirement_type.to_weight_type())
            })
            .try_fold((I80F48::ZERO, I80F48::ZERO), |(ta, tl), res| {
                let (assets, liabilities) = res?;
                let total_assets_sum = ta.checked_add(assets).ok_or_else(math_error!())?;
                let total_liabilities_sum =
                    tl.checked_add(liabilities).ok_or_else(math_error!())?;

                Ok::<_, ProgramError>((total_assets_sum, total_liabilities_sum))
            })?;

        println!(
            "assets {} - liabs: {}",
            total_weighted_assets, total_weighted_liabilities
        );

        check!(
            total_weighted_assets > total_weighted_liabilities,
            MarginfiError::BadAccountHealth
        );

        Ok(())
    }
}

const MAX_LENDING_ACCOUNT_BALANCES: usize = 16;

#[zero_copy]
pub struct LendingAccount {
    pub balances: [Option<Balance>; MAX_LENDING_ACCOUNT_BALANCES],
}

impl LendingAccount {
    pub fn get_balance(&self, mint_pk: &Pubkey, banks: &[Option<Bank>]) -> Option<&Balance> {
        self.balances
            .iter()
            .find(|balance| match balance {
                Some(balance) => {
                    let bank = banks[balance.bank_index as usize];

                    match bank {
                        Some(bank) => bank.mint_pk.eq(mint_pk),
                        None => false,
                    }
                }
                None => false,
            })
            .map(|balance| balance.as_ref().unwrap())
    }

    pub fn get_first_empty_balance(&self) -> Option<usize> {
        self.balances.iter().position(|b| b.is_none())
    }

    pub fn get_active_balances_iter(&self) -> impl Iterator<Item = &Balance> {
        self.balances.iter().filter_map(|b| b.as_ref())
    }
}

#[zero_copy]
pub struct Balance {
    pub bank_index: u8,
    pub deposit_shares: WrappedI80F48,
    pub liability_shares: WrappedI80F48,
}

impl Balance {
    pub fn change_deposit_shares(&mut self, delta: I80F48) -> MarginfiResult {
        let deposit_shares: I80F48 = self.deposit_shares.into();
        self.deposit_shares = deposit_shares
            .checked_add(delta)
            .ok_or_else(math_error!())?
            .into();
        Ok(())
    }

    pub fn change_liability_shares(&mut self, delta: I80F48) -> MarginfiResult {
        let liability_shares: I80F48 = self.liability_shares.into();
        self.liability_shares = liability_shares
            .checked_add(delta)
            .ok_or_else(math_error!())?
            .into();
        Ok(())
    }
}

pub struct BankAccountWrapper<'a> {
    balance: &'a mut Balance,
    bank: &'a mut Bank,
}

impl<'a> BankAccountWrapper<'a> {
    pub fn find_by_mint_or_create<'b>(
        mint: Pubkey,
        lending_pool: &'a mut LendingPool,
        lending_account: &'a mut LendingAccount,
    ) -> MarginfiResult<BankAccountWrapper<'a>> {
        // Find the bank by asset mint pk
        let (bank_index, bank) = lending_pool
            .banks
            .iter_mut()
            .enumerate()
            .filter(|(_, b)| b.is_some())
            .find(|(_, b)| b.unwrap().mint_pk == mint)
            .ok_or_else(|| error!(MarginfiError::BankNotFound))?;

        // Find the user lending account balance by `bank_index`.
        // The balance account might not exist.
        let balance_index = lending_account
            .get_active_balances_iter()
            .position(|b| b.bank_index as usize == bank_index);

        let balance = if let Some(index) = balance_index {
            lending_account
                .balances // active_balances?
                .get_mut(index)
                .ok_or_else(|| error!(MarginfiError::LendingAccountBalanceNotFound))?
        } else {
            let empty_index = lending_account
                .get_first_empty_balance()
                .ok_or_else(|| error!(MarginfiError::LendingAccountBalanceSlotsFull))?;

            lending_account.balances[empty_index] = Some(Balance {
                bank_index: bank_index as u8,
                deposit_shares: I80F48::ZERO.into(),
                liability_shares: I80F48::ZERO.into(),
            });

            lending_account.balances.get_mut(empty_index).unwrap()
        }
        .as_mut()
        .unwrap();

        Ok(Self {
            balance,
            bank: bank.as_mut().unwrap(),
        })
    }

    pub fn find_or_create(
        bank_index: u16,
        lending_pool: &'a mut LendingPool,
        lending_account: &'a mut LendingAccount,
    ) -> MarginfiResult<BankAccountWrapper<'a>> {
        // Find the bank by asset mint pk
        let bank = lending_pool
            .banks
            .get_mut(bank_index as usize)
            .ok_or_else(|| error!(MarginfiError::BankNotFound))?
            .as_mut()
            .ok_or_else(|| error!(MarginfiError::BankNotFound))?;

        // Find the user lending account balance by `bank_index`.
        // The balance account might not exist.
        let balance_index = lending_account
            .get_active_balances_iter()
            .position(|b| b.bank_index as usize == bank_index as usize);

        let balance = if let Some(index) = balance_index {
            lending_account
                .balances
                .get_mut(index)
                .ok_or_else(|| error!(MarginfiError::LendingAccountBalanceNotFound))?
        } else {
            let empty_index = lending_account
                .get_first_empty_balance()
                .ok_or_else(|| error!(MarginfiError::LendingAccountBalanceSlotsFull))?;

            lending_account.balances[empty_index] = Some(Balance {
                bank_index: bank_index as u8,
                deposit_shares: I80F48::ZERO.into(),
                liability_shares: I80F48::ZERO.into(),
            });

            lending_account.balances.get_mut(empty_index).unwrap()
        }
        .as_mut()
        .unwrap();

        Ok(Self { balance, bank })
    }

    pub fn account_deposit(&mut self, amount: I80F48) -> MarginfiResult {
        let balance = &mut self.balance;
        let bank = &mut self.bank;

        let liability_shares: I80F48 = balance.liability_shares.into();

        let liability_value = bank.get_liability_amount(liability_shares)?;

        let (deposit_value_delta, liability_replay_value_delta) = (
            max(
                amount
                    .checked_sub(liability_value)
                    .ok_or_else(math_error!())?,
                I80F48::ZERO,
            ),
            min(liability_value, amount),
        );

        let deposit_shares_delta = bank.get_deposit_shares(deposit_value_delta)?;
        balance.change_deposit_shares(deposit_shares_delta)?;
        bank.change_deposit_shares(deposit_shares_delta)?;

        let liability_shares_delta = bank.get_liability_shares(liability_replay_value_delta)?;
        balance.change_liability_shares(-liability_shares_delta)?;
        bank.change_liability_shares(-liability_shares_delta)?;

        Ok(())
    }

    /// Borrow an asset, will withdraw existing deposits if they exist.
    pub fn account_borrow(&mut self, amount: I80F48) -> MarginfiResult {
        self.account_credit_asset(amount, true)
    }

    /// Withdraw a deposit, will error if there is not enough deposit.
    /// Borrowing is not allowed.
    pub fn account_withdraw(&mut self, amount: I80F48) -> MarginfiResult {
        self.account_credit_asset(amount, false)
    }

    fn account_credit_asset(&mut self, amount: I80F48, allow_borrow: bool) -> MarginfiResult {
        let balance = &mut self.balance;
        let bank = &mut self.bank;

        let deposit_shares: I80F48 = balance.deposit_shares.into();

        let deposit_value = bank.get_deposit_amount(deposit_shares)?;

        let (deposit_remove_value_delta, liability_value_delta) = (
            min(deposit_value, amount),
            max(
                amount
                    .checked_sub(deposit_value)
                    .ok_or_else(math_error!())?,
                I80F48::ZERO,
            ),
        );

        check!(
            allow_borrow || liability_value_delta == I80F48::ZERO,
            MarginfiError::BorrowingNotAllowed
        );

        let deposit_shares_delta = bank.get_deposit_shares(deposit_remove_value_delta)?;
        balance.change_deposit_shares(-deposit_shares_delta)?;
        bank.change_deposit_shares(-deposit_shares_delta)?;

        let liability_shares_delta = bank.get_liability_shares(liability_value_delta)?;
        balance.change_liability_shares(liability_shares_delta)?;
        bank.change_liability_shares(liability_shares_delta)?;

        Ok(())
    }

    pub fn deposit_spl_transfer<'b: 'c, 'c: 'b>(
        &self,
        amount: u64,
        accounts: Transfer<'b>,
        program: AccountInfo<'c>,
    ) -> MarginfiResult {
        self.bank.deposit_spl_transfer(amount, accounts, program)
    }

    pub fn withdraw_spl_transfer<'b: 'c, 'c: 'b>(
        &self,
        amount: u64,
        accounts: Transfer<'b>,
        program: AccountInfo<'c>,
        signer_seeds: &[&[&[u8]]],
    ) -> MarginfiResult {
        self.bank
            .withdraw_spl_transfer(amount, accounts, program, signer_seeds)
    }
}
