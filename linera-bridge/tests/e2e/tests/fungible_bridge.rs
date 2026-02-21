// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test: deploy a fungible token on Linera, transfer tokens to an EVM address,
//! submit the block certificate to FungibleBridge on Anvil, and verify the ERC20 balance.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use alloy::{
    network::EthereumWallet,
    primitives::{Address, U256},
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
    sol,
};
use linera_base::{
    crypto::{CryptoHash, InMemorySigner},
    data_types::{Amount, Blob, BlockHeight, Bytecode, ChainDescription, NetworkDescription, Timestamp},
    identifiers::{AccountOwner, ChainId},
    vm::VmRuntime,
};
use linera_bridge_e2e::{
    compose_file_path, exec_ok, exec_output, start_compose, ANVIL_PRIVATE_KEY,
    LIGHT_CLIENT_ADDRESS,
};
use linera_core::{
    client::{
        create_bytecode_blobs, ChainClient, ChainClientOptions, Client, ListeningMode,
        DEFAULT_CERTIFICATE_DOWNLOAD_BATCH_SIZE, DEFAULT_SENDER_CERTIFICATE_DOWNLOAD_BATCH_SIZE,
    },
    data_types::ClientOutcome,
    environment::{self, wallet::Memory},
    node::CrossChainMessageDelivery,
    DEFAULT_QUORUM_GRACE_PERIOD,
};
use linera_execution::{committee::Committee, Operation, WasmRuntime};
use linera_rpc::node_provider::{NodeOptions, NodeProvider};
use linera_sdk::abis::fungible::{self, FungibleOperation, FungibleTokenAbi};
use linera_storage::{DbStorage, Storage};
use linera_views::backends::memory::{MemoryDatabase, MemoryStoreConfig};

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

sol! {
    #[sol(rpc)]
    interface IFungibleBridge {
        function addBlock(bytes calldata data) external;
    }
}

/// Parse a "Deployed to: 0x..." address from forge create output.
fn parse_deployed_address(output: &str) -> Address {
    for line in output.lines() {
        if let Some(addr) = line.strip_prefix("Deployed to: ") {
            return addr.trim().parse().expect("valid deployed address");
        }
    }
    panic!("Could not find 'Deployed to:' in forge output:\n{output}");
}

type Env = environment::Impl<
    DbStorage<MemoryDatabase>,
    NodeProvider,
    InMemorySigner,
    Memory,
>;

/// Unwrap a `ClientOutcome::Committed` value, panicking on conflict or timeout.
fn unwrap_committed<T>(outcome: ClientOutcome<T>) -> T {
    match outcome {
        ClientOutcome::Committed(value) => value,
        ClientOutcome::Conflict(_) => panic!("unexpected block conflict"),
        ClientOutcome::WaitForTimeout(timeout) => {
            panic!("unexpected timeout: {timeout:?}")
        }
    }
}

/// Create a chain client for a given chain from the core `Client`.
fn make_chain_client(
    client: &Arc<Client<Env>>,
    chain_id: ChainId,
    owner: Option<AccountOwner>,
) -> ChainClient<Env> {
    client.create_chain_client(chain_id, None, BlockHeight::ZERO, None, owner, None)
}

/// Parsed genesis config from the faucet (only the fields we need).
struct FaucetGenesisConfig {
    committee: Committee,
    timestamp: Timestamp,
    chains: Vec<ChainDescription>,
}

impl FaucetGenesisConfig {
    fn admin_chain_id(&self) -> ChainId {
        self.chains[0].id()
    }
}

/// Queries the faucet's genesis config.
async fn faucet_genesis_config(faucet_url: &str) -> FaucetGenesisConfig {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let resp: serde_json::Value = http
        .post(faucet_url)
        .json(&serde_json::json!({"query": "query { genesisConfig }"}))
        .send()
        .await
        .expect("faucet genesis_config request")
        .error_for_status()
        .expect("faucet genesis_config status")
        .json()
        .await
        .expect("faucet genesis_config JSON");

    let genesis = &resp["data"]["genesisConfig"];
    FaucetGenesisConfig {
        chains: serde_json::from_value(genesis["chains"].clone()).expect("parse genesis chains"),
        committee: serde_json::from_value(genesis["committee"].clone())
            .expect("parse genesis committee"),
        timestamp: serde_json::from_value(genesis["timestamp"].clone())
            .expect("parse genesis timestamp"),
    }
}

/// Claims a new chain from the faucet for the given owner.
async fn faucet_claim(faucet_url: &str, owner: &AccountOwner) -> ChainDescription {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let resp: serde_json::Value = http
        .post(faucet_url)
        .json(&serde_json::json!({
            "query": format!("mutation {{ claim(owner: \"{owner}\") }}")
        }))
        .send()
        .await
        .expect("faucet claim request")
        .error_for_status()
        .expect("faucet claim status")
        .json()
        .await
        .expect("faucet claim JSON");

    serde_json::from_value(resp["data"]["claim"].clone()).expect("parse claim response")
}

#[tokio::test]
#[ignore] // Requires pre-built docker images and WASM: `make -C linera-bridge build-all`
async fn test_fungible_bridge_transfers_to_evm() {
    let compose_file = compose_file_path();
    let project_name = "linera-bridge-test";

    let compose = start_compose(&compose_file, project_name).await;

    // ── 1. Create programmatic Linera client ──
    eprintln!("Creating programmatic Linera client...");
    let faucet_url = "http://localhost:8080";
    let genesis = faucet_genesis_config(faucet_url).await;
    let admin_chain_id = genesis.admin_chain_id();
    eprintln!("Admin chain: {admin_chain_id}");

    let config = MemoryStoreConfig {
        max_stream_queries: 10,
        kill_on_drop: true,
    };
    let storage = DbStorage::<MemoryDatabase, _>::maybe_create_and_connect(
        &config,
        "bridge-e2e-test",
        Some(WasmRuntime::default()),
    )
    .await
    .expect("create in-memory storage");

    // Initialize storage with genesis data (committee blob, network description, chains).
    let committee_blob =
        Blob::new_committee(bcs::to_bytes(&genesis.committee).expect("serialize committee"));
    let network_description = NetworkDescription {
        name: String::new(),
        genesis_config_hash: CryptoHash::new(&committee_blob),
        genesis_timestamp: genesis.timestamp,
        genesis_committee_blob_hash: committee_blob.id().hash,
        admin_chain_id,
    };
    storage
        .write_network_description(&network_description)
        .await
        .expect("write network description");
    storage
        .write_blob(&committee_blob)
        .await
        .expect("write committee blob");
    for chain_desc in &genesis.chains {
        storage
            .create_chain(chain_desc.clone())
            .await
            .expect("create genesis chain");
    }

    let mut signer = InMemorySigner::new(None);
    let wallet = Memory::default();

    let node_options = NodeOptions {
        send_timeout: Duration::from_secs(4),
        recv_timeout: Duration::from_secs(4),
        retry_delay: Duration::from_secs(1),
        max_retries: 10,
    };
    let node_provider = NodeProvider::new(node_options);

    let chain_client_options = ChainClientOptions {
        max_pending_message_bundles: 10,
        max_block_limit_errors: 3,
        max_new_events_per_block: 10,
        message_policy: Default::default(),
        cross_chain_message_delivery: CrossChainMessageDelivery::new(true),
        quorum_grace_period: DEFAULT_QUORUM_GRACE_PERIOD,
        blob_download_timeout: Duration::from_secs(1),
        certificate_batch_download_timeout: Duration::from_secs(1),
        certificate_download_batch_size: DEFAULT_CERTIFICATE_DOWNLOAD_BATCH_SIZE,
        sender_certificate_download_batch_size: DEFAULT_SENDER_CERTIFICATE_DOWNLOAD_BATCH_SIZE,
        max_joined_tasks: 100,
        allow_fast_blocks: false,
    };

    let env = environment::Impl {
        storage,
        network: node_provider,
        signer: signer.clone(),
        wallet,
    };
    let client = Arc::new(Client::new(
        env,
        admin_chain_id,
        false,
        vec![],
        "bridge-e2e-test",
        Duration::from_secs(30),
        Duration::from_secs(1),
        chain_client_options,
        Default::default(),
    ));

    // ── 2. Claim chain A from faucet ──
    eprintln!("Claiming chain A from faucet...");
    let public_key_a = signer.generate_new();
    let owner_a = AccountOwner::from(public_key_a);
    let chain_a_desc = faucet_claim(faucet_url, &owner_a).await;
    let chain_a = chain_a_desc.id();

    // Store chain description and initialize chain state in local storage.
    client
        .storage_client()
        .create_chain(chain_a_desc.clone())
        .await
        .expect("create chain A in local storage");

    // Register chain in wallet and track it in chain modes.
    client.wallet().try_insert(
        chain_a,
        linera_core::environment::wallet::Chain::new(
            Some(owner_a),
            chain_a_desc.config().epoch,
            chain_a_desc.timestamp(),
        ),
    );
    client.extend_chain_mode(chain_a, ListeningMode::FullChain);

    let cc_a = make_chain_client(&client, chain_a, Some(owner_a));
    cc_a.synchronize_from_validators()
        .await
        .expect("sync chain A");
    eprintln!("Chain A: {chain_a}, Owner A: {owner_a}");

    // ── 3. Publish and create fungible app on chain A ──
    eprintln!("Publishing fungible module...");
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let wasm_dir = repo_root.join("examples/target/wasm32-unknown-unknown/release");
    let contract_bytecode = Bytecode::load_from_file(wasm_dir.join("fungible_contract.wasm"))
        .expect("load contract bytecode");
    let service_bytecode = Bytecode::load_from_file(wasm_dir.join("fungible_service.wasm"))
        .expect("load service bytecode");

    let (blobs, module_id) =
        create_bytecode_blobs(contract_bytecode, service_bytecode, VmRuntime::Wasm).await;

    let cert = unwrap_committed(
        cc_a.publish_module_blobs(blobs, module_id)
            .await
            .expect("publish module blobs"),
    );
    eprintln!("Module published in block: {:?}", cert.1.inner().block().header.height);

    // Process inbox after publish
    cc_a.synchronize_from_validators()
        .await
        .expect("sync after publish");
    cc_a.process_inbox().await.expect("process inbox after publish");

    eprintln!("Creating fungible application...");
    let params = fungible::Parameters::new("TEST");
    let init_state = fungible::InitialState {
        accounts: BTreeMap::from([(owner_a, Amount::from_tokens(1000))]),
    };
    let (app_id, _cert): (linera_base::identifiers::ApplicationId<FungibleTokenAbi>, _) =
        unwrap_committed(
            cc_a.create_application(
                module_id.with_abi::<FungibleTokenAbi, _, _>(),
                &params,
                &init_state,
                vec![],
            )
            .await
            .expect("create fungible application"),
        );
    let app_id = app_id.forget_abi();
    eprintln!("Application ID: {app_id}");

    // ── 4. Claim chain B (bridge chain) from faucet ──
    eprintln!("Claiming chain B from faucet...");
    let public_key_b = signer.generate_new();
    let owner_b = AccountOwner::from(public_key_b);
    let chain_b_desc = faucet_claim(faucet_url, &owner_b).await;
    let chain_b = chain_b_desc.id();

    // Store chain description and initialize chain state in local storage.
    // This ensures the ChainDescription blob is available when chain A's
    // cross-chain messages need to be delivered to chain B.
    client
        .storage_client()
        .create_chain(chain_b_desc.clone())
        .await
        .expect("create chain B in local storage");

    client.wallet().try_insert(
        chain_b,
        linera_core::environment::wallet::Chain::new(
            Some(owner_b),
            chain_b_desc.config().epoch,
            chain_b_desc.timestamp(),
        ),
    );
    client.extend_chain_mode(chain_b, ListeningMode::FullChain);
    eprintln!("Chain B (bridge): {chain_b}");

    // ── 5. Deploy MockERC20 on Anvil ──
    eprintln!("Deploying MockERC20...");
    let erc20_output = exec_output(
        &compose,
        "foundry-tools",
        &format!(
            "forge create /contracts/MockERC20.sol:MockERC20 \
             --root /contracts --via-ir --optimize \
             --out /tmp/forge-out --cache-path /tmp/forge-cache \
             --rpc-url http://anvil:8545 \
             --broadcast \
             --private-key {ANVIL_PRIVATE_KEY} \
             --constructor-args \"TestToken\" \"TT\" 1000000000000000000000"
        ),
        project_name,
        &compose_file,
    )
    .await;
    let erc20_addr = parse_deployed_address(&erc20_output);
    eprintln!("MockERC20 deployed at: {erc20_addr}");

    // ── 6. Deploy FungibleBridge on Anvil ──
    let app_id_bytes32 = format!("0x{}", app_id.application_description_hash);
    let chain_b_bytes32 = format!("0x{chain_b}");

    eprintln!("Deploying FungibleBridge...");
    let bridge_output = exec_output(
        &compose,
        "foundry-tools",
        &format!(
            "forge create /contracts/FungibleBridge.sol:FungibleBridge \
             --root /contracts --via-ir --optimize \
             --ignored-error-codes 6321 \
             --out /tmp/forge-out --cache-path /tmp/forge-cache \
             --rpc-url http://anvil:8545 \
             --private-key {ANVIL_PRIVATE_KEY} \
             --broadcast \
             --constructor-args \
             0x{LIGHT_CLIENT_ADDRESS} \
             {chain_b_bytes32} \
             0 \
             {app_id_bytes32} \
             {erc20_addr}"
        ),
        project_name,
        &compose_file,
    )
    .await;
    let bridge_addr = parse_deployed_address(&bridge_output);
    eprintln!("FungibleBridge deployed at: {bridge_addr}");

    // ── 7. Fund FungibleBridge with ERC20 tokens ──
    eprintln!("Funding FungibleBridge with ERC20 tokens...");
    exec_ok(
        &compose,
        "foundry-tools",
        &format!(
            "cast send --rpc-url http://anvil:8545 \
             --private-key {ANVIL_PRIVATE_KEY} \
             {erc20_addr} \
             'transfer(address,uint256)(bool)' \
             {bridge_addr} \
             500000000000000000000"
        ),
        project_name,
        &compose_file,
    )
    .await;

    // ── 8. Transfer tokens from chain A to Address20 on chain B ──
    let evm_recipient = "70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let receiver: AccountOwner = format!("0x{evm_recipient}").parse().expect("parse EVM address");

    eprintln!("Sending fungible transfer to Address20 on chain B...");
    let transfer_op = Operation::User {
        application_id: app_id,
        bytes: bcs::to_bytes(&FungibleOperation::Transfer {
            owner: owner_a,
            amount: Amount::from_tokens(100),
            target_account: fungible::Account {
                chain_id: chain_b,
                owner: receiver,
            },
        })
        .expect("serialize transfer operation"),
    };
    let transfer_cert = unwrap_committed(
        cc_a.execute_operations(vec![transfer_op], vec![])
            .await
            .expect("execute transfer"),
    );
    let transfer_block = transfer_cert.inner().block();
    eprintln!(
        "Transfer block height: {:?}, messages: {}, recipients: {:?}",
        transfer_block.header.height,
        transfer_block.body.messages.len(),
        transfer_block.recipients()
    );

    // ── 9. Process inbox on chain B (with retry) ──
    eprintln!("Processing inbox on chain B...");
    let cc_b = make_chain_client(&client, chain_b, Some(owner_b));
    let mut certs = Vec::new();
    for attempt in 1..=5 {
        cc_b.synchronize_from_validators()
            .await
            .expect("sync chain B");
        let (c, _) = cc_b.process_inbox().await.expect("process inbox on chain B");
        if !c.is_empty() {
            certs = c;
            break;
        }
        eprintln!("  process_inbox attempt {attempt}: 0 certificates, retrying after 2s...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!("Processed {} certificate(s) from inbox", certs.len());
    assert!(
        !certs.is_empty(),
        "process_inbox should produce at least one certificate"
    );

    // ── 10. Get certificate bytes for addBlock ──
    let cert = certs.last().unwrap();
    let cert_bytes = bcs::to_bytes(cert).expect("BCS serialize certificate");
    eprintln!("Certificate size: {} bytes", cert_bytes.len());

    // ── 11. Call addBlock() on FungibleBridge ──
    eprintln!("Calling addBlock on FungibleBridge...");
    let rpc_url = "http://localhost:8545".parse().unwrap();
    let evm_signer: PrivateKeySigner = ANVIL_PRIVATE_KEY.parse().unwrap();
    let evm_wallet = EthereumWallet::from(evm_signer);
    let provider = ProviderBuilder::new()
        .wallet(evm_wallet)
        .connect_http(rpc_url);

    let bridge_contract = IFungibleBridge::new(bridge_addr, &provider);
    let tx = bridge_contract
        .addBlock(cert_bytes.into())
        .send()
        .await
        .expect("addBlock transaction send");
    let receipt = tx.get_receipt().await.expect("addBlock receipt");
    eprintln!("addBlock tx: {:?}", receipt.transaction_hash);

    // ── 12. Verify ERC20 balance ──
    let evm_recipient_addr: Address = format!("0x{evm_recipient}").parse().unwrap();
    let erc20_contract = IERC20::new(erc20_addr, &provider);
    let balance = erc20_contract
        .balanceOf(evm_recipient_addr)
        .call()
        .await
        .expect("balanceOf call");
    eprintln!("ERC20 balance of recipient: {balance}");

    // 100 tokens = 100 * 10^18 (Amount uses 18 decimal places)
    let expected_balance = U256::from(100u64) * U256::from(10u64).pow(U256::from(18));
    assert_eq!(
        balance, expected_balance,
        "ERC20 balance should match the transferred amount"
    );

    eprintln!("Test passed! ERC20 balance matches transferred amount.");
}
