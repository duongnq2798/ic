use candid::{candid_method, Nat};
use dfn_candid::{candid, candid_one, CandidOne};
use dfn_core::{
    api::{caller, data_certificate, print, set_certified_data, trap_with},
    over, over_async, over_async_may_reject, over_init, printer, setup, stable, BytesS,
};
use dfn_protobuf::protobuf;
use ic_base_types::{CanisterId, PrincipalId};
use ic_canister_log::{LogEntry, Sink};
use ic_icrc1::{
    endpoints::{StandardRecord, TransferArg, Value},
    Account,
};
use ic_ledger_canister_core::{
    archive::{Archive, ArchiveOptions},
    ledger::{
        apply_transaction, archive_blocks, block_locations, find_block_in_archive, LedgerAccess,
    },
    range_utils,
};
use ic_ledger_core::{
    block::{BlockIndex, BlockType, EncodedBlock},
    timestamp::TimeStamp,
    tokens::{Tokens, DECIMAL_PLACES},
};
use icp_ledger::{
    protobuf, tokens_into_proto, AccountBalanceArgs, AccountIdentifier, ArchiveInfo,
    ArchivedBlocksRange, Archives, BinaryAccountBalanceArgs, Block, BlockArg, BlockRes,
    CandidBlock, Decimals, GetBlocksArgs, IterBlocksArgs, LedgerCanisterInitPayload, Memo, Name,
    Operation, PaymentError, QueryArchiveFn, QueryBlocksResponse, SendArgs, Subaccount, Symbol,
    TipOfChainRes, TotalSupplyArgs, Transaction, TransferArgs, TransferError, TransferFee,
    TransferFeeArgs, MAX_BLOCKS_PER_REQUEST,
};
use ledger_canister::{Ledger, LEDGER, MAX_MESSAGE_SIZE_BYTES};
use num_traits::cast::ToPrimitive;
use std::cell::RefCell;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
    time::Duration,
};

#[derive(Clone)]
struct DebugOutSink;

impl Sink for DebugOutSink {
    fn append(&self, entry: LogEntry) {
        print!("{}", entry.message)
    }
}

/// Initialize the ledger canister
///
/// # Arguments
///
/// * `minting_account` -  The minting canister is given 2^64 - 1 tokens and it
///   then transfers tokens to addresses specified in the initial state.
///   Currently this is the only way to create tokens.
/// * `initial_values` - The list of accounts that will get balances at genesis.
///   This balances are paid out from the minting canister using 'Send'
///   transfers.
/// * `max_message_size_bytes` - The maximum message size that this subnet
/// * `transaction_window` - The [Ledger] transaction window.
/// * `archive_options` - The options of the canister that manages the store of
///   old blocks.
/// * `send_whitelist` - The [Ledger] canister whitelist.
/// * `transfer_fee` - The fee to pay to perform a transaction.
/// * `token_symbol` - Token symbol.
/// * `token_name` - Token name.
#[allow(clippy::too_many_arguments)]
fn init(
    minting_account: AccountIdentifier,
    icrc1_minting_account: Option<Account>,
    initial_values: HashMap<AccountIdentifier, Tokens>,
    max_message_size_bytes: Option<usize>,
    transaction_window: Option<Duration>,
    archive_options: Option<ArchiveOptions>,
    send_whitelist: HashSet<CanisterId>,
    transfer_fee: Option<Tokens>,
    token_symbol: Option<String>,
    token_name: Option<String>,
) {
    print(format!(
        "[ledger] init(): minting account is {}",
        minting_account
    ));
    LEDGER.write().unwrap().from_init(
        initial_values,
        minting_account,
        icrc1_minting_account,
        dfn_core::api::now().into(),
        transaction_window,
        send_whitelist,
        transfer_fee,
        token_symbol,
        token_name,
    );
    match max_message_size_bytes {
        None => {
            print(format!(
                "[ledger] init(): using default maximum message size: {}",
                MAX_MESSAGE_SIZE_BYTES.read().unwrap()
            ));
        }
        Some(max_message_size_bytes) => {
            *MAX_MESSAGE_SIZE_BYTES.write().unwrap() = max_message_size_bytes;
            print(format!(
                "[ledger] init(): using maximum message size: {}",
                max_message_size_bytes
            ));
        }
    }
    set_certified_data(
        &LEDGER
            .read()
            .unwrap()
            .blockchain
            .last_hash
            .map(|h| h.into_bytes())
            .unwrap_or([0u8; 32]),
    );

    if let Some(archive_options) = archive_options {
        LEDGER.write().unwrap().blockchain.archive =
            Arc::new(RwLock::new(Some(Archive::new(archive_options))))
    }
}

#[cfg(feature = "notify-method")]
fn add_payment(
    memo: Memo,
    operation: Operation,
    created_at_time: Option<TimeStamp>,
) -> (BlockIndex, ic_ledger_core::block::HashOf<EncodedBlock>) {
    let (height, hash) = ledger_canister::add_payment(memo, operation, created_at_time);
    set_certified_data(&hash.into_bytes());
    (height, hash)
}

/// This is the only operation that changes the state of the canister blocks and
/// balances after init. This creates a payment from the caller's account. It
/// returns the index of the resulting transaction
///
/// # Arguments
///
/// * `memo` -  A 8 byte "message" you can attach to transactions to help the
///   receiver disambiguate transactions.
/// * `amount` - The number of Tokens the recipient gets. The number of Tokens
///   withdrawn is equal to the amount + the fee.
/// * `fee` - The maximum fee that the sender is willing to pay. If the required
///   fee is greater than this the transaction will be rejected otherwise the
///   required fee will be paid.
/// * `from_subaccount` - The subaccount you want to draw funds from.
/// * `to` - The account you want to send the funds to.
/// * `created_at_time`: When the transaction has been created. If not set then
///   now is used.
async fn send(
    memo: Memo,
    amount: Tokens,
    fee: Tokens,
    from_subaccount: Option<Subaccount>,
    to: AccountIdentifier,
    created_at_time: Option<TimeStamp>,
) -> Result<BlockIndex, TransferError> {
    let caller_principal_id = caller();

    if !LEDGER.read().unwrap().can_send(&caller_principal_id) {
        panic!("Sending from {} is not allowed", caller_principal_id);
    }

    let from = AccountIdentifier::new(caller_principal_id, from_subaccount);
    let minting_acc = LEDGER
        .read()
        .unwrap()
        .minting_account_id
        .expect("Minting canister id not initialized");

    let transfer = if from == minting_acc {
        assert_eq!(fee, Tokens::ZERO, "Fee for minting should be zero");
        assert_ne!(
            to, minting_acc,
            "It is illegal to mint to a minting_account"
        );
        Operation::Mint { to, amount }
    } else if to == minting_acc {
        assert_eq!(fee, Tokens::ZERO, "Fee for burning should be zero");
        let min_burn_amount = LEDGER.read().unwrap().transfer_fee;
        if amount < min_burn_amount {
            panic!("Burns lower than {} are not allowed", min_burn_amount);
        }
        Operation::Burn { from, amount }
    } else {
        let transfer_fee = LEDGER.read().unwrap().transfer_fee;
        if fee != transfer_fee {
            return Err(TransferError::BadFee {
                expected_fee: transfer_fee,
            });
        }
        Operation::Transfer {
            from,
            to,
            amount,
            fee,
        }
    };
    let (height, hash) = match LEDGER
        .write()
        .unwrap()
        .add_payment(memo, transfer, created_at_time)
    {
        Ok((height, hash)) => (height, hash),
        Err(PaymentError::TransferError(transfer_error)) => return Err(transfer_error),
        Err(PaymentError::Reject(msg)) => panic!("{}", msg),
    };
    set_certified_data(&hash.into_bytes());

    // Don't put anything that could ever trap after this call or people using this
    // endpoint. If something did panic the payment would appear to fail, but would
    // actually succeed on chain.
    let max_msg_size = *MAX_MESSAGE_SIZE_BYTES.read().unwrap();
    archive_blocks::<Access>(DebugOutSink, max_msg_size as u64).await;
    Ok(height)
}

async fn icrc1_send(
    memo: Option<ic_icrc1::Memo>,
    amount: Tokens,
    fee: Option<Nat>,
    from_account: Account,
    to: AccountIdentifier,
    created_at_time: Option<TimeStamp>,
) -> Result<BlockIndex, ic_icrc1::endpoints::TransferError> {
    let from = AccountIdentifier::from(from_account);
    let minting_acc = LEDGER
        .read()
        .unwrap()
        .minting_account_id
        .expect("Minting canister id not initialized");
    let now = TimeStamp::from_nanos_since_unix_epoch(ic_cdk::api::time());
    let (operation, effective_fee) = if to == minting_acc {
        if fee.is_some() && fee.as_ref() != Some(&Nat::from(0u64)) {
            return Err(ic_icrc1::endpoints::TransferError::BadFee {
                expected_fee: Nat::from(0u64),
            });
        }
        let ledger = LEDGER.read().unwrap();
        let balance = ledger.balances.account_balance(&from);
        let min_burn_amount = ledger.transfer_fee.min(balance);
        if amount < min_burn_amount {
            return Err(ic_icrc1::endpoints::TransferError::BadBurn {
                min_burn_amount: Nat::from(min_burn_amount.get_e8s()),
            });
        }
        if amount == Tokens::ZERO {
            return Err(ic_icrc1::endpoints::TransferError::BadBurn {
                min_burn_amount: Nat::from(ledger.transfer_fee.get_e8s()),
            });
        }
        (Operation::Burn { from, amount }, Tokens::ZERO)
    } else if from == minting_acc {
        if fee.is_some() && fee.as_ref() != Some(&Nat::from(0u64)) {
            return Err(ic_icrc1::endpoints::TransferError::BadFee {
                expected_fee: Nat::from(0u64),
            });
        }
        (Operation::Mint { to, amount }, Tokens::ZERO)
    } else {
        let expected_fee = LEDGER.read().unwrap().transfer_fee;
        if fee.is_some() && fee.as_ref() != Some(&Nat::from(expected_fee.get_e8s())) {
            return Err(ic_icrc1::endpoints::TransferError::BadFee {
                expected_fee: Nat::from(expected_fee.get_e8s()),
            });
        }
        (
            Operation::Transfer {
                from,
                to,
                amount,
                fee: expected_fee,
            },
            expected_fee,
        )
    };

    let block_index = {
        let mut ledger = LEDGER.write().unwrap();
        let tx = Transaction {
            operation,
            memo: Memo(0),
            icrc1_memo: memo.map(|x| x.0),
            created_at_time,
        };
        let (block_index, hash) = apply_transaction(&mut *ledger, tx, now, effective_fee)
            .map_err(ic_icrc1::endpoints::TransferError::from)?;

        set_certified_data(&hash.into_bytes());

        block_index
    };

    let max_msg_size = *MAX_MESSAGE_SIZE_BYTES.read().unwrap();
    archive_blocks::<Access>(DebugOutSink, max_msg_size as u64).await;
    Ok(block_index)
}

thread_local! {
    static NOTIFY_METHOD_CALLS: RefCell<u64> = RefCell::new(0);
}

/// You can notify a canister that you have made a payment to it. The
/// payment must have been made to the account of a canister and from the
/// callers account. You cannot notify a canister about a transaction it has
/// already been successfully notified of. If the canister rejects the
/// notification call it is not considered to have been notified.
///
/// # Arguments
///
/// * `block_height` -  The height of the block you would like to send a
///   notification about.
/// * `max_fee` - The fee of the payment.
/// * `from_subaccount` - The subaccount that made the payment.
/// * `to_canister` - The canister that received the payment.
/// * `to_subaccount` - The subaccount that received the payment.
/// * `notify_using_protobuf` - Whether the notification should be encoded using
///   protobuf or candid.
#[cfg(feature = "notify-method")]
pub async fn notify(
    block_height: BlockIndex,
    max_fee: Tokens,
    from_subaccount: Option<Subaccount>,
    to_canister: CanisterId,
    to_subaccount: Option<Subaccount>,
    notify_using_protobuf: bool,
) -> Result<BytesS, String> {
    use dfn_core::api::{call_bytes_with_cleanup, call_with_cleanup, Funds};
    use dfn_protobuf::ProtoBuf;
    use on_wire::IntoWire;

    NOTIFY_METHOD_CALLS.with(|n| *n.borrow_mut() += 1);

    let caller_principal_id = caller();

    if !LEDGER.read().unwrap().can_send(&caller_principal_id) {
        panic!("Notifying from {} is not allowed", caller_principal_id);
    }

    if !LEDGER.read().unwrap().can_be_notified(&to_canister) {
        panic!(
            "Notifying non-whitelisted canister is not allowed: {}",
            to_canister
        );
    }

    let expected_from = AccountIdentifier::new(caller_principal_id, from_subaccount);

    let expected_to = AccountIdentifier::new(to_canister.get(), to_subaccount);

    let transfer_fee = LEDGER.read().unwrap().transfer_fee;

    if max_fee != transfer_fee {
        panic!("Transfer fee should be {}", transfer_fee);
    }

    let raw_block: EncodedBlock =
        match block(block_height).unwrap_or_else(|| panic!("Block {} not found", block_height)) {
            Ok(raw_block) => raw_block,
            Err(cid) => {
                print(format!(
                    "Searching canister {} for block {}",
                    cid, block_height
                ));
                // Lookup the block on the archive
                let BlockRes(res) = call_with_cleanup(cid, "get_block_pb", protobuf, block_height)
                    .await
                    .map_err(|e| format!("Failed to fetch block {}", e.1))?;
                res.ok_or("Block not found")?
                    .map_err(|c| format!("Tried to redirect lookup a second time to {}", c))?
            }
        };

    let block = Block::decode(raw_block).unwrap();

    let (from, to, amount) = match block.transaction().operation {
        Operation::Transfer {
            from, to, amount, ..
        } => (from, to, amount),
        _ => panic!("Notification failed transfer must be of type send"),
    };

    assert_eq!(
        (from, to),
        (expected_from, expected_to),
        "sender and recipient must match the specified block"
    );

    let transaction_notification_args = icp_ledger::TransactionNotification {
        from: caller_principal_id,
        from_subaccount,
        to: to_canister,
        to_subaccount,
        block_height,
        amount,
        memo: block.transaction().memo,
    };

    let block_timestamp = block.timestamp();

    ledger_canister::change_notification_state(block_height, block_timestamp, true)
        .expect("Notification failed");

    // This transaction provides an on chain record that a notification was
    // attempted
    let transfer = Operation::Transfer {
        from: expected_from,
        to: expected_to,
        amount: Tokens::ZERO,
        fee: max_fee,
    };

    // While this payment has been made here, it isn't actually committed until you
    // make an inter canister call. As such we don't reject without rollback until
    // an inter-canister call has definitely been made
    add_payment(Memo(block_height), transfer, None);

    let response = if notify_using_protobuf {
        let bytes = ProtoBuf(transaction_notification_args)
            .into_bytes()
            .expect("transaction notification serialization failed");
        call_bytes_with_cleanup(
            to_canister,
            "transaction_notification_pb",
            &bytes[..],
            Funds::zero(),
        )
        .await
    } else {
        let bytes = candid::encode_one(transaction_notification_args)
            .expect("transaction notification serialization failed");
        call_bytes_with_cleanup(
            to_canister,
            "transaction_notification",
            &bytes[..],
            Funds::zero(),
        )
        .await
    };
    // Don't panic after here or the notification might look like it succeeded
    // when actually it failed

    // propagate the response/rejection from 'to_canister' if it's shorter than this
    // length.
    // This could be done better because we still read in the whole
    // String/Vec as is
    pub const MAX_LENGTH: usize = 8192;

    match response {
        Ok(bs) => {
            if bs.len() > MAX_LENGTH {
                let caller = caller();
                Err(format!(
                    "Notification succeeded, but the canister '{}' returned too large of a response",
                    caller,
                ))
            } else {
                Ok(BytesS(bs))
            }
        }
        Err((_code, err)) => {
            // It may be that by the time this callback is made the block will have been
            // garbage collected. That is fine because we don't inspect the
            // response here.
            let _ =
                ledger_canister::change_notification_state(block_height, block_timestamp, false);
            if err.len() > MAX_LENGTH {
                let caller = caller();
                Err(format!(
                    "Notification failed, but the canister '{}' returned too large of a response",
                    caller,
                ))
            } else {
                Err(format!("Notification failed with message '{}'", err))
            }
        }
    }
}

/// This gives you the index of the last block added to the chain
/// together with certification
fn tip_of_chain() -> TipOfChainRes {
    let last_block_idx = &LEDGER
        .read()
        .unwrap()
        .blockchain
        .chain_length()
        .checked_sub(1)
        .unwrap();
    let certification = data_certificate();
    TipOfChainRes {
        certification,
        tip_index: *last_block_idx,
    }
}

// This is going away and being replaced by getblocks
fn block(block_index: BlockIndex) -> Option<Result<EncodedBlock, CanisterId>> {
    let state = LEDGER.read().unwrap();
    if block_index < state.blockchain.num_archived_blocks() {
        // The block we are looking for better be in the archive because it has
        // a height smaller than the number of blocks we've archived so far
        let result =
            find_block_in_archive(&*state, block_index).expect("block not found in the archive");
        Some(Err(result))
    // Or the block may be in the ledger, or the block may not exist
    } else {
        print(format!(
            "[ledger] Checking the ledger for block [{}]",
            block_index
        ));
        state.blockchain.get(block_index).cloned().map(Ok)
    }
}

/// Get an account balance.
/// If the account does not exist it will return 0 Tokens
fn account_balance(account: AccountIdentifier) -> Tokens {
    LEDGER.read().unwrap().balances.account_balance(&account)
}

#[candid_method(query, rename = "icrc1_balance_of")]
fn icrc1_balance_of(acc: Account) -> Nat {
    Nat::from(account_balance(AccountIdentifier::from(acc)).get_e8s())
}

#[candid_method(query, rename = "icrc1_supported_standards")]
fn icrc1_supported_standards() -> Vec<StandardRecord> {
    vec![StandardRecord {
        name: "ICRC-1".to_string(),
        url: "https://github.com/dfinity/ICRC-1".to_string(),
    }]
}

#[candid_method(query, rename = "icrc1_minting_account")]
fn icrc1_minting_account() -> Option<Account> {
    LEDGER.read().unwrap().icrc1_minting_account
}

#[candid_method(query, rename = "transfer_fee")]
fn transfer_fee(_: TransferFeeArgs) -> TransferFee {
    LEDGER.read().unwrap().transfer_fee()
}

#[candid_method(query, rename = "icrc1_metadata")]
fn icrc1_metadata() -> Vec<(String, Value)> {
    vec![
        Value::entry("icrc1:decimals", DECIMAL_PLACES as u64),
        Value::entry("icrc1:name", LEDGER.read().unwrap().token_name.to_string()),
        Value::entry(
            "icrc1:symbol",
            LEDGER.read().unwrap().token_symbol.to_string(),
        ),
        Value::entry("icrc1:fee", LEDGER.read().unwrap().transfer_fee.get_e8s()),
    ]
}

#[candid_method(query, rename = "icrc1_fee")]
fn icrc1_fee() -> Nat {
    Nat::from(LEDGER.read().unwrap().transfer_fee.get_e8s())
}

/// The total number of Tokens not inside the minting canister
fn total_supply() -> Tokens {
    LEDGER.read().unwrap().balances.total_supply()
}

#[candid_method(query, rename = "icrc1_total_supply")]
fn icrc1_total_supply() -> Nat {
    Nat::from(LEDGER.read().unwrap().balances.total_supply().get_e8s())
}

#[candid_method(query, rename = "symbol")]
fn token_symbol() -> Symbol {
    Symbol {
        symbol: LEDGER.read().unwrap().token_symbol.clone(),
    }
}

#[candid_method(query, rename = "name")]
fn token_name() -> Name {
    Name {
        name: LEDGER.read().unwrap().token_name.clone(),
    }
}

#[candid_method(query)]
fn icrc1_name() -> String {
    LEDGER.read().unwrap().token_name.clone()
}

#[candid_method(query, rename = "icrc1_symbol")]
fn icrc1_symbol() -> String {
    LEDGER.read().unwrap().token_symbol.to_string()
}

#[candid_method(query, rename = "decimals")]
fn token_decimals() -> Decimals {
    Decimals {
        decimals: DECIMAL_PLACES,
    }
}

#[candid_method(query, rename = "icrc1_decimals")]
fn icrc1_decimals() -> u8 {
    debug_assert!(ic_ledger_core::tokens::DECIMAL_PLACES <= u8::MAX as u32);
    ic_ledger_core::tokens::DECIMAL_PLACES as u8
}

#[candid_method(init)]
fn canister_init(arg: LedgerCanisterInitPayload) {
    init(
        arg.minting_account,
        arg.icrc1_minting_account,
        arg.initial_values,
        arg.max_message_size_bytes,
        arg.transaction_window,
        arg.archive_options,
        arg.send_whitelist,
        arg.transfer_fee,
        arg.token_symbol,
        arg.token_name,
    )
}

#[export_name = "canister_init"]
fn main() {
    over_init(|CandidOne(arg)| canister_init(arg))
}

#[export_name = "canister_post_upgrade"]
fn post_upgrade() {
    over_init(|_: BytesS| {
        let mut ledger = LEDGER.write().unwrap();
        *ledger = ciborium::de::from_reader(stable::StableReader::new())
            .expect("Decoding stable memory failed");

        ledger.maximum_number_of_accounts = 28_000_000;

        set_certified_data(
            &ledger
                .blockchain
                .last_hash
                .map(|h| h.into_bytes())
                .unwrap_or([0u8; 32]),
        );
    })
}

#[export_name = "canister_pre_upgrade"]
fn pre_upgrade() {
    setup::START.call_once(|| {
        printer::hook();
    });

    let ledger = LEDGER
        .read()
        // This should never happen, but it's better to be safe than sorry
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ciborium::ser::into_writer(&*ledger, stable::StableWriter::new())
        .expect("failed to write ledger state to stable memory");
}

struct Access;

impl LedgerAccess for Access {
    type Ledger = Ledger;

    fn with_ledger<R>(f: impl FnOnce(&Self::Ledger) -> R) -> R {
        let ledger_guard = LEDGER.try_read().expect("Failed to get ledger read lock");
        f(&ledger_guard)
    }

    fn with_ledger_mut<R>(f: impl FnOnce(&mut Self::Ledger) -> R) -> R {
        let mut ledger = LEDGER.write().expect("Failed to get ledger write lock");
        f(&mut ledger)
    }
}

/// Canister endpoints
#[export_name = "canister_update send_pb"]
fn send_() {
    over_async(
        protobuf,
        |SendArgs {
             memo,
             amount,
             fee,
             from_subaccount,
             to,
             created_at_time,
         }| async move {
            send(memo, amount, fee, from_subaccount, to, created_at_time)
                .await
                .unwrap_or_else(|e| {
                    trap_with(&e.to_string());
                    unreachable!()
                })
        },
    );
}

#[candid_method(update, rename = "send_dfx")]
async fn send_dfx(arg: SendArgs) -> BlockIndex {
    transfer_candid(arg.into()).await.unwrap()
}

/// Do not use call this from code, this is only here so dfx has something to
/// call when making a payment. This will be changed in ways that are not
/// backwards compatible with previous interfaces.
///
/// I STRONGLY recommend that you use "send_pb" instead.
#[export_name = "canister_update send_dfx"]
fn send_dfx_() {
    over_async(candid_one, send_dfx);
}

#[cfg(feature = "notify-method")]
#[export_name = "canister_update notify_pb"]
fn notify_() {
    use dfn_core::endpoint::over_async_may_reject_explicit;
    use dfn_protobuf::ProtoBuf;

    // we use over_init because it doesn't reply automatically so we can do explicit
    // replies in the callback
    over_async_may_reject_explicit(
        |ProtoBuf(icp_ledger::NotifyCanisterArgs {
             block_height,
             max_fee,
             from_subaccount,
             to_canister,
             to_subaccount,
         })| async move {
            notify(
                block_height,
                max_fee,
                from_subaccount,
                to_canister,
                to_subaccount,
                true,
            )
            .await
        },
    );
}

#[candid_method(update, rename = "transfer")]
async fn transfer_candid(arg: TransferArgs) -> Result<BlockIndex, TransferError> {
    let to_account = AccountIdentifier::from_address(arg.to).unwrap_or_else(|e| {
        trap_with(&format!("Invalid account identifier: {}", e));
        unreachable!()
    });
    send(
        arg.memo,
        arg.amount,
        arg.fee,
        arg.from_subaccount,
        to_account,
        arg.created_at_time,
    )
    .await
}

#[candid_method(update, rename = "icrc1_transfer")]
async fn icrc1_transfer(arg: TransferArg) -> Result<Nat, ic_icrc1::endpoints::TransferError> {
    let to = AccountIdentifier::from(arg.to);
    let from_account = Account {
        owner: PrincipalId::from(ic_cdk::api::caller()),
        subaccount: arg.from_subaccount,
    };
    let amount = match arg.amount.0.to_u64() {
        Some(n) => Tokens::from_e8s(n),
        None => {
            // No one can have so many tokens
            let balance =
                Nat::from(account_balance(AccountIdentifier::from(from_account)).get_e8s());
            assert!(balance < arg.amount);
            return Err(ic_icrc1::endpoints::TransferError::InsufficientFunds { balance });
        }
    };
    let created_at_time = arg
        .created_at_time
        .map(TimeStamp::from_nanos_since_unix_epoch);
    Ok(Nat::from(
        icrc1_send(arg.memo, amount, arg.fee, from_account, to, created_at_time).await?,
    ))
}

#[export_name = "canister_update transfer"]
fn transfer() {
    over_async(candid_one, transfer_candid)
}

#[export_name = "canister_update icrc1_transfer"]
fn icrc1_transfer_candid() {
    over_async_may_reject(candid_one, |arg: TransferArg| async {
        if !LEDGER.read().unwrap().can_send(&caller()) {
            return Err("Anonymous principal cannot hold tokens on the ledger.".to_string());
        }

        Ok(icrc1_transfer(arg).await)
    })
}

/// See caveats of use on send_dfx
#[cfg(feature = "notify-method")]
#[export_name = "canister_update notify_dfx"]
fn notify_dfx_() {
    use dfn_core::endpoint::over_async_may_reject_explicit;

    // we use over_init because it doesn't reply automatically so we can do explicit
    // replies in the callback
    over_async_may_reject_explicit(
        |CandidOne(icp_ledger::NotifyCanisterArgs {
             block_height,
             max_fee,
             from_subaccount,
             to_canister,
             to_subaccount,
         })| {
            notify(
                block_height,
                max_fee,
                from_subaccount,
                to_canister,
                to_subaccount,
                false,
            )
        },
    );
}

#[export_name = "canister_query block_pb"]
fn block_() {
    over(protobuf, |BlockArg(height)| BlockRes(block(height)));
}

#[export_name = "canister_query tip_of_chain_pb"]
fn tip_of_chain_() {
    over(protobuf, |protobuf::TipOfChainRequest {}| tip_of_chain());
}

#[export_name = "canister_query get_archive_index_pb"]
fn get_archive_index_() {
    over(protobuf, |()| {
        let state = LEDGER.read().unwrap();
        let entries = match &state
            .blockchain
            .archive
            .try_read()
            .expect("Failed to get lock on archive")
            .as_ref()
        {
            None => vec![],
            Some(archive) => archive
                .index()
                .into_iter()
                .map(
                    |((height_from, height_to), canister_id)| protobuf::ArchiveIndexEntry {
                        height_from,
                        height_to,
                        canister_id: Some(canister_id.get()),
                    },
                )
                .collect(),
        };
        protobuf::ArchiveIndexResponse { entries }
    });
}

#[export_name = "canister_query account_balance_pb"]
fn account_balance_() {
    over(protobuf, |AccountBalanceArgs { account }| {
        tokens_into_proto(account_balance(account))
    })
}

#[candid_method(query, rename = "account_balance")]
fn account_balance_candid_(arg: BinaryAccountBalanceArgs) -> Tokens {
    let account = AccountIdentifier::from_address(arg.account).unwrap_or_else(|e| {
        trap_with(&format!("Invalid account identifier: {}", e));
        unreachable!()
    });
    account_balance(account)
}

#[export_name = "canister_query account_balance"]
fn account_balance_candid() {
    over(candid_one, account_balance_candid_)
}

#[candid_method(query, rename = "account_balance_dfx")]
fn account_balance_dfx_(args: AccountBalanceArgs) -> Tokens {
    account_balance(args.account)
}

/// See caveats of use on send_dfx
#[export_name = "canister_query account_balance_dfx"]
fn account_balance_dfx() {
    over(candid_one, account_balance_dfx_);
}

#[export_name = "canister_query icrc1_balance_of"]
fn icrc1_balance_of_candid() {
    over(candid_one, icrc1_balance_of)
}

#[export_name = "canister_query transfer_fee"]
fn transfer_fee_candid() {
    over(candid_one, transfer_fee)
}

#[export_name = "canister_query icrc1_fee"]
fn icrc1_fee_candid() {
    over(candid_one, |()| icrc1_fee())
}

#[export_name = "canister_query transfer_fee_pb"]
fn transfer_fee_() {
    over(protobuf, transfer_fee)
}

#[export_name = "canister_query symbol"]
fn token_symbol_candid() {
    over(candid_one, |()| token_symbol())
}

#[export_name = "canister_query name"]
fn token_name_candid() {
    over(candid_one, |()| token_name())
}

#[export_name = "canister_query icrc1_name"]
fn icrc1_name_candid() {
    over(candid_one, |()| icrc1_name())
}

#[export_name = "canister_query decimals"]
fn token_decimals_candid() {
    over(candid_one, |()| token_decimals())
}

#[export_name = "canister_query icrc1_decimals"]
fn icrc1_decimals_candid() {
    over(candid_one, |()| icrc1_decimals())
}

#[export_name = "canister_query total_supply_pb"]
fn total_supply_() {
    over(protobuf, |_: TotalSupplyArgs| {
        tokens_into_proto(total_supply())
    })
}

#[export_name = "canister_query icrc1_total_supply"]
fn icrc1_total_supply_candid() {
    over(candid_one, |()| icrc1_total_supply())
}

/// Get multiple blocks by *offset into the container* (not BlockIndex) and
/// length. Note that this simply iterates the blocks available in the Ledger
/// without taking into account the archive. For example, if the ledger contains
/// blocks with heights [100, 199] then iter_blocks(0, 1) will return the block
/// with height 100.
#[export_name = "canister_query iter_blocks_pb"]
fn iter_blocks_() {
    over(protobuf, |IterBlocksArgs { start, length }| {
        let blocks = &LEDGER.read().unwrap().blockchain.blocks;
        icp_ledger::iter_blocks(blocks, start, length)
    });
}

/// Get multiple blocks by BlockIndex and length. If the query is outside the
/// range stored in the Node the result is an error.
#[export_name = "canister_query get_blocks_pb"]
fn get_blocks_() {
    over(protobuf, |GetBlocksArgs { start, length }| {
        let blockchain = &LEDGER.read().unwrap().blockchain;
        let start_offset = blockchain.num_archived_blocks();
        icp_ledger::get_blocks(&blockchain.blocks, start_offset, start, length)
    });
}

#[export_name = "canister_query icrc1_supported_standards"]
fn icrc1_supported_standards_candid() {
    over(candid_one, |()| icrc1_supported_standards())
}

#[candid_method(query, rename = "query_blocks")]
fn query_blocks(GetBlocksArgs { start, length }: GetBlocksArgs) -> QueryBlocksResponse {
    let ledger = LEDGER.read().unwrap();
    let locations = block_locations(&*ledger, start, length);

    let local_blocks = range_utils::take(&locations.local_blocks, MAX_BLOCKS_PER_REQUEST);

    let blocks: Vec<CandidBlock> = ledger
        .blockchain
        .block_slice(local_blocks.clone())
        .iter()
        .map(|enc_block| -> CandidBlock {
            Block::decode(enc_block.clone())
                .expect("bug: failed to decode encoded block")
                .into()
        })
        .collect();

    let archived_blocks = locations
        .archived_blocks
        .into_iter()
        .map(|(canister_id, slice)| ArchivedBlocksRange {
            start: slice.start,
            length: range_utils::range_len(&slice),
            callback: QueryArchiveFn {
                canister_id,
                method: "get_blocks".to_string(),
            },
        })
        .collect();

    let chain_length = ledger.blockchain.chain_length();

    QueryBlocksResponse {
        chain_length,
        certificate: dfn_core::api::data_certificate().map(serde_bytes::ByteBuf::from),
        blocks,
        first_block_index: local_blocks.start as BlockIndex,
        archived_blocks,
    }
}

#[export_name = "canister_query query_blocks"]
fn query_blocks_() {
    over(candid_one, query_blocks)
}

#[export_name = "canister_query icrc1_minting_account"]
fn icrc1_minting_account_candid() {
    over(candid_one, |()| icrc1_minting_account())
}

#[export_name = "canister_query icrc1_symbol"]
fn icrc1_symbol_candid() {
    over(candid_one, |()| icrc1_symbol())
}

#[candid_method(query, rename = "archives")]
fn archives() -> Archives {
    let ledger_guard = LEDGER.try_read().expect("Failed to get ledger read lock");
    let archive_guard = ledger_guard.blockchain.archive.read().unwrap();
    let archives = archive_guard
        .as_ref()
        .iter()
        .flat_map(|archive| {
            archive
                .nodes()
                .iter()
                .map(|cid| ArchiveInfo { canister_id: *cid })
        })
        .collect();
    Archives { archives }
}

#[export_name = "canister_query get_nodes"]
fn get_nodes_() {
    over(candid, |()| {
        archives()
            .archives
            .iter()
            .map(|archive| archive.canister_id)
            .collect::<Vec<CanisterId>>()
    });
}

#[export_name = "canister_query archives"]
fn archives_candid() {
    over(candid_one, |()| archives());
}

#[export_name = "canister_query icrc1_metadata"]
fn icrc1_metadata_candid() {
    over(candid_one, |()| icrc1_metadata())
}

fn encode_metrics(w: &mut ic_metrics_encoder::MetricsEncoder<Vec<u8>>) -> std::io::Result<()> {
    let ledger = LEDGER.try_read().map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to get a LEDGER for read: {}", err),
        )
    })?;

    w.encode_gauge(
        "ledger_max_message_size_bytes",
        *MAX_MESSAGE_SIZE_BYTES.read().unwrap() as f64,
        "Maximum inter-canister message size in bytes.",
    )?;
    w.encode_gauge(
        "ledger_stable_memory_pages",
        dfn_core::api::stable_memory_size_in_pages() as f64,
        "Size of the stable memory allocated by this canister measured in 64K Wasm pages.",
    )?;
    w.encode_gauge(
        "ledger_stable_memory_bytes",
        (dfn_core::api::stable_memory_size_in_pages() * 64 * 1024) as f64,
        "Size of the stable memory allocated by this canister.",
    )?;
    w.encode_gauge(
        "ledger_transactions_by_hash_cache_entries",
        ledger.transactions_by_hash_len() as f64,
        "Total number of entries in the transactions_by_hash cache.",
    )?;
    w.encode_gauge(
        "ledger_transactions_by_height_entries",
        ledger.transactions_by_height_len() as f64,
        "Total number of entries in the transaction_by_height queue.",
    )?;
    w.encode_gauge(
        "ledger_blocks",
        ledger.blockchain.blocks.len() as f64,
        "Total number of blocks stored in the main memory.",
    )?;
    // This value can go down -- the number is increased before archiving, and if
    // archiving fails it is decremented.
    w.encode_gauge(
        "ledger_archived_blocks",
        ledger.blockchain.num_archived_blocks as f64,
        "Total number of blocks sent to the archive.",
    )?;
    w.encode_gauge(
        "ledger_balances_token_pool",
        ledger.balances.token_pool.get_tokens() as f64,
        "Total number of Tokens in the pool.",
    )?;
    w.encode_gauge(
        "ledger_balance_store_entries",
        ledger.balances.store.len() as f64,
        "Total number of accounts in the balance store.",
    )?;
    w.encode_gauge(
        "ledger_most_recent_block_time_seconds",
        ledger.blockchain.last_timestamp.as_nanos_since_unix_epoch() as f64 / 1_000_000_000.0,
        "IC timestamp of the most recent block.",
    )?;
    w.encode_gauge(
        "ledger_notify_method_calls",
        NOTIFY_METHOD_CALLS.with(|n| *n.borrow()) as f64,
        "Total number of calls to the notify-method method.",
    )?;
    Ok(())
}

#[export_name = "canister_query http_request"]
fn http_request() {
    dfn_http_metrics::serve_metrics(encode_metrics);
}

candid::export_service!();

#[export_name = "canister_query __get_candid_interface_tmp_hack"]
fn get_canidid_interface() {
    over(candid_one, |()| -> String { __export_service() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::utils::{service_compatible, CandidSource};
    use std::path::PathBuf;

    #[test]
    fn check_candid_interface_compatibility() {
        let new_interface = __export_service();
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        for candid_file in ["../ledger.did", "../../icrc1/ledger/icrc1.did"].iter() {
            let old_interface = manifest_dir.join(candid_file);

            service_compatible(
                CandidSource::Text(&new_interface),
                CandidSource::File(old_interface.as_path()),
            )
            .unwrap_or_else(|e| {
                panic!(
                    "the ledger interface is not compatible with {}: {:?}",
                    old_interface.display(),
                    e
                )
            });
        }
    }
    // FI-510 Backwards compatibility testing for Candid and Protobuf
    #[test]
    fn check_candid_interface_backwards_compatibility() {
        candid::export_service!();
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let old_candid_file = manifest_dir.join("./ledger_candid_backwards_compatible.did");
        let new_candid_file = manifest_dir.join("../ledger.did");
        service_compatible(
            CandidSource::File(new_candid_file.as_path()),
            CandidSource::File(old_candid_file.as_path()),
        )
        .unwrap_or_else(|e| {
            panic!(
                "the ledger interface is not backwards compatible {}: {:?}",
                new_candid_file.display(),
                e
            )
        });
    }
}
