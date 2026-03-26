#![no_std]

extern crate alloc;

mod access;
mod events;
mod storage;
pub mod types;

use access::{require_admin, require_not_paused, require_relayer};
use events::emit;
use soroban_sdk::{contract, contractimpl, Address, Env, String as SorobanString, Vec};
use storage::{assets, deposits, dlq, max_deposit, relayers, settlements};
use types::{DlqEntry, Event, Settlement, Transaction, TransactionStatus};

#[contract]
pub struct SynapseContract;

#[contractimpl]
impl SynapseContract {
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&storage::StorageKey::Admin) {
            panic!("already initialised");
        }
        admin.require_auth();
        storage::admin::set(&env, &admin);
        emit(&env, Event::Initialized(admin));
    }

    pub fn grant_relayer(env: Env, caller: Address, relayer: Address) {
        require_not_paused(&env);
        let zero_addr = Address::from_string(&SorobanString::from_str(
            &env,
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
        ));
        if relayer == zero_addr { panic!("invalid relayer address") }
        require_admin(&env, &caller);
        relayers::add(&env, &relayer);
        emit(&env, Event::RelayerGranted(relayer));
    }

    pub fn revoke_relayer(env: Env, caller: Address, relayer: Address) {
        require_not_paused(&env);
        require_admin(&env, &caller);
        if !relayers::has(&env, &relayer) { panic!("address is not a relayer") }
        relayers::remove(&env, &relayer);
        emit(&env, Event::RelayerRevoked(relayer));
    }

    pub fn transfer_admin(env: Env, caller: Address, new_admin: Address) {
        require_not_paused(&env);
        require_admin(&env, &caller);
        storage::admin::set(&env, &new_admin);
    }

    pub fn pause(env: Env, caller: Address) {
        require_admin(&env, &caller);
        storage::pause::set(&env, true);
    }

    pub fn unpause(env: Env, caller: Address) {
        require_admin(&env, &caller);
        storage::pause::set(&env, false);
    }

    pub fn add_asset(env: Env, caller: Address, asset_code: SorobanString) {
        require_not_paused(&env);
        require_admin(&env, &caller);
        assets::add(&env, &asset_code);
        emit(&env, Event::AssetAdded(asset_code));
    }

    pub fn remove_asset(env: Env, caller: Address, asset_code: SorobanString) {
        require_not_paused(&env);
        require_admin(&env, &caller);
        assets::remove(&env, &asset_code);
        emit(&env, Event::AssetRemoved(asset_code));
    }

    pub fn set_max_deposit(env: Env, caller: Address, amount: i128) {
        require_admin(&env, &caller);
        if amount <= 0 { panic!("max deposit must be positive") }
        max_deposit::set(&env, amount);
    }

    pub fn get_max_deposit(env: Env) -> Option<i128> {
        max_deposit::get(&env)
    }

    pub fn register_deposit(
        env: Env,
        caller: Address,
        anchor_transaction_id: SorobanString,
        stellar_account: Address,
        amount: i128,
        asset_code: SorobanString,
        memo: Option<SorobanString>,
    ) -> SorobanString {
        require_not_paused(&env);
        require_relayer(&env, &caller);
        assets::require_allowed(&env, &asset_code);
        if let Some(max) = max_deposit::get(&env) {
            if amount > max { panic!("amount exceeds max deposit") }
        }
        if let Some(existing) = deposits::find_by_anchor_id(&env, &anchor_transaction_id) {
            return existing;
        }
        let tx = Transaction::new(
            &env,
            anchor_transaction_id.clone(),
            stellar_account,
            caller,
            amount,
            asset_code,
            memo,
        );
        let id = tx.id.clone();
        deposits::save(&env, &tx);
        deposits::index_anchor_id(&env, &anchor_transaction_id, &id);
        emit(&env, Event::DepositRegistered(id.clone(), anchor_transaction_id));
        id
    }

    pub fn mark_processing(env: Env, caller: Address, tx_id: SorobanString) {
        require_not_paused(&env);
        require_relayer(&env, &caller);
        let mut tx = deposits::get(&env, &tx_id);
        if tx.status != TransactionStatus::Pending { panic!("invalid status transition") }
        tx.status = TransactionStatus::Processing;
        tx.updated_ledger = env.ledger().sequence();
        deposits::save(&env, &tx);
        emit(&env, Event::StatusUpdated(tx_id, TransactionStatus::Processing));
    }

    pub fn mark_completed(env: Env, caller: Address, tx_id: SorobanString) {
        require_not_paused(&env);
        require_relayer(&env, &caller);
        let mut tx = deposits::get(&env, &tx_id);
        tx.status = TransactionStatus::Completed;
        tx.updated_ledger = env.ledger().sequence();
        deposits::save(&env, &tx);
        emit(&env, Event::StatusUpdated(tx_id, TransactionStatus::Completed));
    }

    pub fn mark_failed(env: Env, caller: Address, tx_id: SorobanString, error_reason: SorobanString) {
        require_not_paused(&env);
        require_relayer(&env, &caller);
        let mut tx = deposits::get(&env, &tx_id);
        tx.status = TransactionStatus::Failed;
        tx.updated_ledger = env.ledger().sequence();
        deposits::save(&env, &tx);
        let entry = DlqEntry::new(&env, tx_id.clone(), error_reason.clone());
        dlq::push(&env, &entry);
        emit(&env, Event::MovedToDlq(tx_id, error_reason));
    }

    pub fn retry_dlq(env: Env, caller: Address, tx_id: SorobanString) {
        require_not_paused(&env);
        let tx = deposits::get(&env, &tx_id);
        let admin = storage::admin::get(&env);
        if caller != admin && caller != tx.relayer {
            panic!("not admin or original relayer")
        }
        let mut entry = dlq::get(&env, &tx_id).expect("dlq entry not found");
        let mut tx = tx;
        tx.status = TransactionStatus::Pending;
        tx.updated_ledger = env.ledger().sequence();
        entry.retry_count += 1;
        entry.last_retry_ledger = env.ledger().sequence();
        deposits::save(&env, &tx);
        dlq::push(&env, &entry);
        emit(&env, Event::StatusUpdated(tx_id, TransactionStatus::Pending));
    }

    pub fn finalize_settlement(
        env: Env,
        caller: Address,
        asset_code: SorobanString,
        tx_ids: Vec<SorobanString>,
        total_amount: i128,
        period_start: u64,
        period_end: u64,
    ) -> SorobanString {
        require_not_paused(&env);
        require_relayer(&env, &caller);
        if period_start > period_end { panic!("period_start must be <= period_end") }
        let n = tx_ids.len();
        let mut i: u32 = 0;
        while i < n {
            let tx_id = tx_ids.get(i).unwrap();
            let tx = deposits::get(&env, &tx_id);
            if tx.status != TransactionStatus::Completed { panic!("transaction not completed"); }
            if tx.settlement_id.len() > 0 { panic!("transaction already settled"); }
            i += 1;
        }
        let s = Settlement::new(&env, asset_code.clone(), tx_ids.clone(), total_amount, period_start, period_end);
        let id = s.id.clone();
        let mut j: u32 = 0;
        while j < n {
            let tx_id = tx_ids.get(j).unwrap();
            let mut tx = deposits::get(&env, &tx_id);
            tx.settlement_id = id.clone();
            deposits::save(&env, &tx);
            emit(&env, Event::Settled(tx_id, id.clone()));
            j += 1;
        }
        settlements::save(&env, &s);
        emit(&env, Event::SettlementFinalized(id.clone(), asset_code, total_amount));
        id
    }

    pub fn get_admin(env: Env) -> Address { storage::admin::get(&env) }
    pub fn is_paused(env: Env) -> bool { storage::pause::is_paused(&env) }
    pub fn get_transaction(env: Env, tx_id: SorobanString) -> Transaction { deposits::get(&env, &tx_id) }
    pub fn get_settlement(env: Env, settlement_id: SorobanString) -> Settlement { settlements::get(&env, &settlement_id) }
    pub fn is_asset_allowed(env: Env, asset_code: SorobanString) -> bool { assets::is_allowed(&env, &asset_code) }
    pub fn is_relayer(env: Env, address: Address) -> bool { relayers::has(&env, &address) }
}