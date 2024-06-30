//! tests for anvil specific logic

use crate::utils::ethers_http_provider;
use anvil::{spawn, NodeConfig};
use ethers::{prelude::Middleware, types::Address};
use foundry_common::types::ToAlloy;

#[tokio::test(flavor = "multi_thread")]
async fn test_can_change_mining_mode() {
    let (api, handle) = spawn(NodeConfig::test()).await;
    let provider = ethers_http_provider(&handle.http_endpoint());

    assert!(api.anvil_get_auto_mine().unwrap());

    let num = provider.get_block_number().await.unwrap();
    assert_eq!(num.as_u64(), 0);

    api.anvil_set_interval_mining(1).unwrap();
    assert!(!api.anvil_get_auto_mine().unwrap());
    // changing the mining mode will instantly mine a new block
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let num = provider.get_block_number().await.unwrap();
    assert_eq!(num.as_u64(), 0);

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let num = provider.get_block_number().await.unwrap();
    assert_eq!(num.as_u64(), 1);

    // assert that no block is mined when the interval is set to 0
    api.anvil_set_interval_mining(0).unwrap();
    assert!(!api.anvil_get_auto_mine().unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let num = provider.get_block_number().await.unwrap();
    assert_eq!(num.as_u64(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn can_get_default_dev_keys() {
    let (_api, handle) = spawn(NodeConfig::test()).await;
    let provider = ethers_http_provider(&handle.http_endpoint());

    let dev_accounts = handle.dev_accounts().collect::<Vec<_>>();
    let accounts = provider
        .get_accounts()
        .await
        .unwrap()
        .into_iter()
        .map(ToAlloy::to_alloy)
        .collect::<Vec<_>>();
    assert_eq!(dev_accounts, accounts);
}

#[tokio::test(flavor = "multi_thread")]
async fn can_set_empty_code() {
    let (api, _handle) = spawn(NodeConfig::test()).await;
    let addr = Address::random();
    api.anvil_set_code(addr.to_alloy(), Vec::new().into()).await.unwrap();
    let code = api.get_code(addr.to_alloy(), None).await.unwrap();
    assert!(code.as_ref().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_can_set_genesis_timestamp() {
    let genesis_timestamp = 1000u64;
    let (_api, handle) =
        spawn(NodeConfig::test().with_genesis_timestamp(genesis_timestamp.into())).await;
    let provider = ethers_http_provider(&handle.http_endpoint());

    assert_eq!(genesis_timestamp, provider.get_block(0).await.unwrap().unwrap().timestamp.as_u64());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_can_use_default_genesis_timestamp() {
    let (_api, handle) = spawn(NodeConfig::test()).await;
    let provider = ethers_http_provider(&handle.http_endpoint());

    assert_ne!(0u64, provider.get_block(0).await.unwrap().unwrap().timestamp.as_u64());
}
