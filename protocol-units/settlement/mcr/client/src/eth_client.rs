use crate::send_eth_transaction::InsufficentFunds;
use crate::send_eth_transaction::SendTransactionErrorRule;
use crate::send_eth_transaction::UnderPriced;
use crate::send_eth_transaction::VerifyRule;
use crate::{CommitmentStream, McrSettlementClientOperations};
use alloy::pubsub::PubSubFrontend;
use alloy_network::Ethereum;
use alloy_network::EthereumWallet;
use alloy_primitives::Address;
use alloy_primitives::U256;
use alloy::providers::fillers::ChainIdFiller;
use alloy::providers::fillers::FillProvider;
use alloy::providers::fillers::GasFiller;
use alloy::providers::fillers::JoinFill;
use alloy::providers::fillers::NonceFiller;
use alloy::providers::fillers::WalletFiller;
use alloy::providers::{ProviderBuilder, Provider, RootProvider};
use alloy::signers::local::PrivateKeySigner;
use alloy_sol_types::sol;
use alloy_transport::BoxTransport;
use alloy_transport_ws::WsConnect;
use anyhow::Context;
use mcr_settlement_config::Config;
use movement_types::BlockCommitment;
use movement_types::{Commitment, Id};
use serde_json::Value as JsonValue;
use std::array::TryFromSliceError;
use std::fs;
use std::path::Path;
use thiserror::Error;
use tokio_stream::StreamExt;

#[derive(Error, Debug)]
pub enum McrEthConnectorError {
	#[error(
		"MCR Settlement Transaction fails because gas estimation is too high. Estimated gas:{0} gas limit:{1}"
	)]
	GasLimitExceed(u128, u128),
	#[error("MCR Settlement Transaction fails because account funds are insufficient. error:{0}")]
	InsufficientFunds(String),
	#[error("MCR Settlement Transaction send failed because :{0}")]
	SendTransactionError(#[from] alloy_contract::Error),
	#[error("MCR Settlement Transaction send failed during its execution :{0}")]
	RpcTransactionExecution(String),
	#[error("MCR Settlement BlockAccepted event notification error :{0}")]
	EventNotificationError(#[from] alloy_sol_types::Error),
	#[error("MCR Settlement BlockAccepted event notification stream close")]
	EventNotificationStreamClosed,
}

// Note: we prefer using the ABI because the [`sol!`](alloy_sol_types::sol) macro, when used with smart contract code directly, will not handle inheritance.
sol!(
	#[allow(missing_docs)]
	#[sol(rpc)]
	MCR,
	"abis/MCR.json"
);


// Note: we prefer using the ABI because the [`sol!`](alloy_sol_types::sol) macro, when used with smart contract code directly, will not handle inheritance.
sol!(
	#[allow(missing_docs)]
	#[sol(rpc)]
	MovementStaking,
	"abis/MovementStaking.json"
);


// Note: we prefer using the ABI because the [`sol!`](alloy_sol_types::sol) macro, when used with smart contract code directly, will not handle inheritance.
sol!(
	#[allow(missing_docs)]
	#[sol(rpc)]
	MOVEToken,
	"abis/MOVEToken.json"
);

pub struct Client<P> {
	rpc_provider: P,
	ws_provider: RootProvider<PubSubFrontend>,
	pub signer_address: Address,
	contract_address: Address,
	send_transaction_error_rules: Vec<Box<dyn VerifyRule>>,
	gas_limit: u64,
	send_transaction_retries: u32,
}

impl
	Client<
		FillProvider<
			JoinFill<
				JoinFill<
					JoinFill<JoinFill<alloy::providers::Identity, GasFiller>, NonceFiller>,
					ChainIdFiller,
				>,
				WalletFiller<EthereumWallet>,
			>,
			RootProvider<BoxTransport>,
			BoxTransport,
			Ethereum,
		>,
	>
{
	pub async fn build_with_config(config: Config) -> Result<Self, anyhow::Error> {
		let signer_private_key = config.settle.signer_private_key.clone();
		let signer = signer_private_key.parse::<PrivateKeySigner>()?;
		let signer_address = signer.address();
		let contract_address = config.settle.mcr_contract_address.parse()?;
		let rpc_url = config.eth_rpc_connection_url();
		let ws_url = config.eth_ws_connection_url();
		let rpc_provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.wallet(EthereumWallet::from(signer))
			.on_builtin(&rpc_url)
			.await.context(
				"Failed to create the RPC provider for the MCR settlement client",
			)?;

		let mut client = Client::build_with_provider(
			rpc_provider,
			ws_url,
			signer_address,
			contract_address,
			config.transactions.gas_limit,
			config.transactions.transaction_send_retries,
		)
		.await?;
		Ok(client)
	}
}

impl<P> Client<P> {
	async fn build_with_provider<S>(
		rpc_provider: P,
		ws_url: S,
		signer_address: Address,
		contract_address: Address,
		gas_limit: u64,
		send_transaction_retries: u32,
	) -> Result<Self, anyhow::Error>
	where
		P: Provider + Clone,
		S: Into<String>,
	{
		let ws = WsConnect::new(ws_url);

		let ws_provider = ProviderBuilder::new().on_ws(ws).await?;

		let rule1: Box<dyn VerifyRule> = Box::new(SendTransactionErrorRule::<UnderPriced>::new());
		let rule2: Box<dyn VerifyRule> =
			Box::new(SendTransactionErrorRule::<InsufficentFunds>::new());
		let send_transaction_error_rules = vec![rule1, rule2];

		Ok(Client {
			rpc_provider,
			ws_provider,
			signer_address,
			contract_address,
			send_transaction_error_rules,
			gas_limit,
			send_transaction_retries,
		})
	}
}

#[async_trait::async_trait]
impl<P> McrSettlementClientOperations for Client<P>
where
	P: Provider + Clone,
{
	async fn post_block_commitment(
		&self,
		block_commitment: BlockCommitment,
	) -> Result<(), anyhow::Error> {
		let contract = MCR::new(self.contract_address, &self.rpc_provider);

		let eth_block_commitment = MCR::BlockCommitment {
			// Currently, to simplify the API, we'll say 0 is uncommitted all other numbers are legitimate heights
			height: U256::from(block_commitment.height),
			commitment: alloy_primitives::FixedBytes(block_commitment.commitment.0),
			blockId: alloy_primitives::FixedBytes(block_commitment.block_id.0),
		};

		let call_builder = contract.submitBlockCommitment(eth_block_commitment);

		crate::send_eth_transaction::send_transaction(
			call_builder,
			&self.send_transaction_error_rules,
			self.send_transaction_retries,
			self.gas_limit as u128,
		)
		.await
	}

	async fn post_block_commitment_batch(
		&self,
		block_commitments: Vec<BlockCommitment>,
	) -> Result<(), anyhow::Error> {
		let contract = MCR::new(self.contract_address, &self.rpc_provider);

		let eth_block_commitment: Vec<_> = block_commitments
			.into_iter()
			.map(|block_commitment| {
				Ok(MCR::BlockCommitment {
					// Currently, to simplify the API, we'll say 0 is uncommitted all other numbers are legitimate heights
					height: U256::from(block_commitment.height),
					commitment: alloy_primitives::FixedBytes(block_commitment.commitment.0),
					blockId: alloy_primitives::FixedBytes(block_commitment.block_id.0),
				})
			})
			.collect::<Result<Vec<_>, TryFromSliceError>>()?;

		let call_builder = contract.submitBatchBlockCommitment(eth_block_commitment);

		crate::send_eth_transaction::send_transaction(
			call_builder,
			&self.send_transaction_error_rules,
			self.send_transaction_retries,
			self.gas_limit as u128,
		)
		.await
	}

	async fn stream_block_commitments(&self) -> Result<CommitmentStream, anyhow::Error> {
		// Register to contract BlockCommitmentSubmitted event

		let contract = MCR::new(self.contract_address, &self.ws_provider);
		let event_filter = contract.BlockAccepted_filter().watch().await?;

		let stream = event_filter.into_stream().map(|event| {
			event
				.and_then(|(commitment, _)| {
					let height = commitment.height.try_into().map_err(
						|err: alloy::primitives::ruint::FromUintError<u64>| {
							alloy_sol_types::Error::Other(err.to_string().into())
						},
					)?;
					Ok(BlockCommitment {
						height,
						block_id: Id(commitment.blockHash.0),
						commitment: Commitment(commitment.stateCommitment.0),
					})
				})
				.map_err(|err| McrEthConnectorError::EventNotificationError(err).into())
		});
		Ok(Box::pin(stream) as CommitmentStream)
	}

	async fn get_commitment_at_height(
		&self,
		height: u64,
	) -> Result<Option<BlockCommitment>, anyhow::Error> {
		let contract = MCR::new(self.contract_address, &self.ws_provider);
		let MCR::getAcceptedCommitmentAtBlockHeightReturn { _0: commitment } = contract
			.getAcceptedCommitmentAtBlockHeight(U256::from(height))
			.call()
			.await?;
		
		let return_height: u64 = commitment.height.try_into().context(
			"Failed to convert the commitment height from U256 to u64",
		)?;
		// Commitment with height 0 mean not found
		Ok((return_height != 0).then_some(BlockCommitment {
			height: commitment.height.try_into().context(
				"Failed to convert the commitment height from U256 to u64",
			)?,
			block_id: Id(commitment.blockId.into()),
			commitment: Commitment(commitment.commitment.into()),
		}))
	}

	async fn get_max_tolerable_block_height(&self) -> Result<u64, anyhow::Error> {
		let contract = MCR::new(self.contract_address, &self.ws_provider);
		let MCR::getMaxTolerableBlockHeightReturn { _0: block_height } =
			contract.getMaxTolerableBlockHeight().call().await?;
		Ok(block_height.try_into().context(
			"Failed to convert the max tolerable block height from U256 to u64",
		)?)
	}
}

pub struct AnvilAddressEntry {
	pub address: String,
	pub private_key: String,
}

/// Read the Anvil config file keys and return all address/private keys.
pub fn read_anvil_json_file_addresses<P: AsRef<Path>>(
	anvil_conf_path: P,
) -> Result<Vec<AnvilAddressEntry>, anyhow::Error> {
	let file_content = fs::read_to_string(anvil_conf_path)?;

	let json_value: JsonValue = serde_json::from_str(&file_content)?;

	// Extract the available_accounts and private_keys fields.
	let available_accounts_iter = json_value["available_accounts"]
		.as_array()
		.expect("Available_accounts should be an array")
		.iter()
		.map(|v| {
			let s = v.as_str().expect("Available_accounts elements should be strings");
			s.to_owned()
		});

	let private_keys_iter = json_value["private_keys"]
		.as_array()
		.expect("Private_keys should be an array")
		.iter()
		.map(|v| {
			let s = v.as_str().expect("Private_keys elements should be strings");
			s.to_owned()
		});

	let res = available_accounts_iter
		.zip(private_keys_iter)
		.map(|(address, private_key)| AnvilAddressEntry { address, private_key })
		.collect::<Vec<_>>();
	Ok(res)
}