use std::sync::Arc;

use crate::node::{Node, ThreadNode};
use framework::config::{GenesisExt, TESTING_INIT_BALANCE, TESTING_INIT_PLEDGE};
use framework::load_test_config;
use testlib::runtime_utils::{alice_account, bob_account};
use unc_chain_configs::Genesis;
use unc_crypto::{InMemorySigner, KeyType};
use unc_jsonrpc::RpcInto;
use unc_network::tcp;
use unc_o11y::testonly::init_integration_logger;
use unc_parameters::{RuntimeConfig, RuntimeConfigStore};
use unc_primitives::account::AccessKey;
use unc_primitives::errors::{InvalidAccessKeyError, InvalidTxError};
use unc_primitives::transaction::{
    Action, AddKeyAction, CreateAccountAction, SignedTransaction, TransferAction,
};
use unc_primitives::version::PROTOCOL_VERSION;

fn start_node() -> ThreadNode {
    init_integration_logger();
    let genesis = Genesis::test(vec![alice_account(), bob_account()], 1);
    let mut unc_config = load_test_config("alice", tcp::ListenerAddr::reserve_for_test(), genesis);
    unc_config.client_config.skip_sync_wait = true;

    let mut node = ThreadNode::new(unc_config);
    node.start();
    node
}

#[test]
fn test_check_tx_error_log() {
    let node = start_node();
    let signer = Arc::new(InMemorySigner::from_seed(alice_account(), KeyType::ED25519, "alice"));
    let block_hash = node.user().get_best_block_hash().unwrap();
    let tx = SignedTransaction::from_actions(
        1,
        bob_account(),
        "test".parse().unwrap(),
        &*signer,
        vec![
            Action::CreateAccount(CreateAccountAction {}),
            Action::Transfer(TransferAction { deposit: 1_000 }),
            Action::AddKey(Box::new(AddKeyAction {
                public_key: signer.public_key.clone(),
                access_key: AccessKey::full_access(),
            })),
        ],
        block_hash,
    );

    let tx_result = node.user().commit_transaction(tx).unwrap_err();
    assert_eq!(
        tx_result,
        InvalidTxError::InvalidAccessKeyError(InvalidAccessKeyError::AccessKeyNotFound {
            account_id: bob_account(),
            public_key: signer.public_key.clone().into()
        })
        .rpc_into()
    );
}

#[test]
fn test_deliver_tx_error_log() {
    let node = start_node();
    let runtime_config_store = RuntimeConfigStore::new(None);
    let runtime_config = runtime_config_store.get_config(PROTOCOL_VERSION);
    let fee_helper = testlib::fees_utils::FeeHelper::new(
        RuntimeConfig::clone(&runtime_config),
        node.genesis().config.min_gas_price,
    );
    let signer = Arc::new(InMemorySigner::from_seed(alice_account(), KeyType::ED25519, "alice"));
    let block_hash = node.user().get_best_block_hash().unwrap();
    let cost = fee_helper.create_account_transfer_full_key_cost_no_reward();
    let tx = SignedTransaction::from_actions(
        1,
        alice_account(),
        "test".parse().unwrap(),
        &*signer,
        vec![
            Action::CreateAccount(CreateAccountAction {}),
            Action::Transfer(TransferAction { deposit: TESTING_INIT_BALANCE + 1 }),
            Action::AddKey(Box::new(AddKeyAction {
                public_key: signer.public_key.clone(),
                access_key: AccessKey::full_access(),
            })),
        ],
        block_hash,
    );

    let tx_result = node.user().commit_transaction(tx).unwrap_err();
    assert_eq!(
        tx_result,
        InvalidTxError::NotEnoughBalance {
            signer_id: alice_account(),
            balance: TESTING_INIT_BALANCE - TESTING_INIT_PLEDGE,
            cost: TESTING_INIT_BALANCE + 1 + cost
        }
        .rpc_into()
    );
}