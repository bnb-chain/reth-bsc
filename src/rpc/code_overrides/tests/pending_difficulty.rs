//! Derived-pending difficulty regressions through the production JSON-RPC handlers.
//! The provider has a fixed latest block and returns None for actual pending blocks.
//! Calls pass through JSON-RPC dispatch and the production BSC RPC/EVM implementations.
use super::*;

const READ_DIFFICULTY: &[u8] = &hex!("4460005260206000f3");
// if (opcode_0x44 > 2) STOP; else SSTORE(0, 1); STOP.
const DIFFICULTY_GAS_BRANCH: &[u8] = &hex!("60024411600d576001600055005b00");

fn header(difficulty: u64) -> alloy_consensus::Header {
    alloy_consensus::Header {
        number: 40_000_000,
        timestamp: NOW,
        gas_limit: 140_000_000,
        base_fee_per_gas: Some(0),
        difficulty: U256::from(difficulty),
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        ..Default::default()
    }
}

macro_rules! dispatch {
    ($module:expr, $method:expr, $params:expr) => {{
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": $method, "params": $params,
        });
        let (response, _) = $module.raw_json_request(&request.to_string(), 1).await.unwrap();
        serde_json::from_str::<serde_json::Value>(response.get()).unwrap()
    }};
}

fn output(response: &serde_json::Value) -> U256 {
    let bytes: Bytes = serde_json::from_value(response["result"].clone())
        .unwrap_or_else(|error| panic!("{error}: {response}"));
    value(&bytes)
}

#[tokio::test]
async fn pending_difficulty_is_stable_and_matches_latest() {
    for difficulty in [1, 2] {
        let (api, _) =
            rpc!(chain forks_at(&[BscHardfork::Jenner], NOW - 1), header header(difficulty));
        assert!(api.0.provider().pending_block_and_receipts().unwrap().is_none());
        let module = api.into_rpc();
        let state = overrides(PROXY, READ_DIFFICULTY);
        let req = request(PROXY, &[]);
        for _ in 0..3 {
            let latest = dispatch!(module, "eth_call", serde_json::json!([req, "latest", state]));
            let pending = dispatch!(module, "eth_call", serde_json::json!([req, "pending", state]));
            assert_eq!(output(&latest), U256::from(difficulty));
            assert_eq!(output(&pending), U256::from(difficulty));
        }
        let default_block = dispatch!(module, "eth_call", serde_json::json!([req, null, state]));
        assert_eq!(output(&default_block), U256::from(difficulty));
        for block_override in [
            serde_json::json!({"prevRandao": B256::from(U256::from(7))}),
            serde_json::json!({"difficulty": "0x7"}),
        ] {
            let result = dispatch!(
                module,
                "eth_call",
                serde_json::json!([req, "pending", state, block_override])
            );
            assert_eq!(output(&result), U256::from(7));
        }
    }
}

#[tokio::test]
async fn pending_difficulty_gas_estimate_covers_header_based_execution() {
    for difficulty in [1, 2] {
        let (api, _) =
            rpc!(chain forks_at(&[BscHardfork::Jenner], NOW - 1), header header(difficulty));
        assert!(api.0.provider().pending_block_and_receipts().unwrap().is_none());
        let module = api.into_rpc();
        let state = overrides(PROXY, DIFFICULTY_GAS_BRANCH);
        let req = TransactionRequest { to: Some(PROXY.into()), ..Default::default() };
        let pending =
            dispatch!(module, "eth_estimateGas", serde_json::json!([req, "pending", state]));
        let latest =
            dispatch!(module, "eth_estimateGas", serde_json::json!([req, "latest", state]));
        let pending_gas: U256 = serde_json::from_value(pending["result"].clone()).unwrap();
        let latest_gas: U256 = serde_json::from_value(latest["result"].clone()).unwrap();
        assert_eq!(pending_gas, latest_gas);
        assert!(pending_gas > U256::from(40_000));
        let limited = TransactionRequest { gas: Some(pending_gas.to::<u64>()), ..req.clone() };
        let pending_call =
            dispatch!(module, "eth_call", serde_json::json!([limited, "pending", state]));
        let latest_call =
            dispatch!(module, "eth_call", serde_json::json!([limited, "latest", state]));
        assert_eq!(pending_call["result"], "0x");
        assert_eq!(latest_call["result"], "0x", "{latest_call}");
    }
}

#[tokio::test]
async fn pending_difficulty_survives_local_block_assembly_and_cache() {
    use reth_primitives_traits::SealedHeader;
    use reth_rpc_eth_api::helpers::LoadPendingBlock;

    for difficulty in [1, 2] {
        let parent = header(difficulty);
        let (api, _) = rpc!(chain forks_at(&[BscHardfork::Jenner], NOW - 1), header parent.clone());
        let parent = SealedHeader::seal_slow(parent);
        let env = api.0.pending_block_env_and_cfg().unwrap().evm_env;
        let pending = api
            .0
            .build_pool_pending_block(parent.clone(), env.clone())
            .await
            .unwrap()
            .expect("locally assembled pending block");
        let block = pending.block();
        assert_eq!(block.header().difficulty, U256::from(difficulty));
        assert_eq!(block.header().mix_hash, B256::ZERO);
        let header_env = api.0.evm_config().evm_env(block.header()).unwrap();
        assert_eq!(header_env.block_env.prevrandao(), Some(U256::from(difficulty).into()));
        assert_eq!(header_env.block_env.milli_remainder, 0);
        let cached = api.0.build_pool_pending_block(parent, env).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(block, cached.block()), "the second request uses the cached block");
    }
}
