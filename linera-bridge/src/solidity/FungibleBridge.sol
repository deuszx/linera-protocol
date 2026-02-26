// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.0;

import "BridgeTypes.sol";
import "FungibleTypes.sol";
import "Microchain.sol";

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

/// Bridges ERC20 tokens from a Linera microchain to Ethereum.
/// When a Credit message is received targeting an Ethereum address,
/// the contract transfers tokens from its own balance to the recipient.
contract FungibleBridge is Microchain {
    bytes32 public immutable applicationId;
    IERC20 public immutable token;
    uint256 public depositNonce;

    /// Emitted when ERC-20 tokens are moved into EVM bridge custody for a Linera credit.
    /// @param source_chain_id EVM chain ID where this deposit transaction happened (e.g. Base mainnet ID).
    /// @param target_chain_id Linera microchain ID that should receive the credit message.
    /// @param target_application_id Linera application ID of the fungible app that should process the deposit.
    /// @param target_account_owner Linera account owner bytes32 (e.g. Ed25519 owner/public-key hash) to credit.
    /// @param token ERC-20 token contract address deposited into custody on this EVM chain.
    /// @param amount Token amount to credit 1:1 on Linera.
    event DepositInitiated(
        uint256 source_chain_id,
        bytes32 target_chain_id,
        bytes32 target_application_id,
        bytes32 target_account_owner,
        address token,
        uint256 amount
    );

    constructor(
        address _lightClient,
        bytes32 _chainId,
        uint64 _nextExpectedHeight,
        bytes32 _applicationId,
        address _token
    )
        Microchain(_lightClient, _chainId, _nextExpectedHeight)
    {
        applicationId = _applicationId;
        token = IERC20(_token);
    }

    /// Deposits ERC-20 tokens into bridge custody and emits a canonical EVM->Linera deposit event.
    /// @param targetChainId Linera destination microchain ID that should receive the minted/credited balance.
    /// @param targetApplicationId Linera destination application ID (the fungible app instance on that chain).
    /// @param targetAccountOwner Linera owner bytes32 identifying which account on the target chain is credited.
    /// @param amount ERC-20 amount pulled from msg.sender and locked in this contract as custody.
    function transferToLinera(
        bytes32 targetChainId,
        bytes32 targetApplicationId,
        bytes32 targetAccountOwner,
        uint256 amount
    ) external {
        require(token.transferFrom(msg.sender, address(this), amount), "token transferFrom failed");

        unchecked {
            depositNonce += 1;
        }

        emit DepositInitiated(
            block.chainid,
            targetChainId,
            targetApplicationId,
            targetAccountOwner,
            address(token),
            amount
        );
    }

    function _onBlock(BridgeTypes.Block memory blockValue) internal override {
        for (uint i = 0; i < blockValue.body.transactions.length; i++) {
            BridgeTypes.Transaction memory txn = blockValue.body.transactions[i];
            // choice==0 is ReceiveMessages
            if (txn.choice != 0) continue;

            BridgeTypes.IncomingBundle memory bundle = txn.receive_messages;
            for (uint j = 0; j < bundle.bundle.messages.length; j++) {
                BridgeTypes.PostedMessage memory posted = bundle.bundle.messages[j];
                // choice==1 is User
                if (posted.message.choice != 1) continue;
                if (posted.message.user.application_id.application_description_hash.value != applicationId) continue;

                FungibleTypes.Message memory msg_ =
                    FungibleTypes.bcs_deserialize_Message(posted.message.user.bytes_);

                // choice==0 is Credit
                if (msg_.choice != 0) continue;

                FungibleTypes.Message_Credit memory credit = msg_.credit;
                // choice==2 is Address20 (Ethereum address)
                if (credit.target.choice != 2) continue;
                address target = address(credit.target.address20);
                require(token.transfer(target, credit.amount.value), "token transfer failed");
            }
        }
    }
}
