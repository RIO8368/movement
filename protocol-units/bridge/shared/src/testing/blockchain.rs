use futures::{channel::mpsc, task::AtomicWaker, Future, Stream, StreamExt};
use std::{
	collections::HashMap,
	pin::Pin,
	task::{Context, Poll},
};

pub use self::{
	client::AbstractBlockchainClient,
	counterparty_contract::{CounterpartyCall, SmartContractCounterparty},
	initiator_contract::{InitiatorCall, SmartContractInitiator},
};

use super::rng::RngSeededClone;
use crate::types::{
	Amount, BridgeAddressType, BridgeHashType, BridgeTransferDetails, BridgeTransferId,
	GenUniqueHash, LockDetails,
};

pub mod client;
pub mod counterparty_contract;
pub mod hasher;
pub mod initiator_contract;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbstractBlockchainEvent<A, H> {
	Noop,
	BridgeTransferInitiated(BridgeTransferDetails<A, H>),
	BridgeTransferAssetsLocked(LockDetails<A, H>),
}

#[derive(Debug)]
pub enum Transaction<A, H> {
	Initiator(InitiatorCall<A, H>),
	Counterparty(CounterpartyCall<A, H>),
}

#[derive(Debug)]
pub struct AbstractBlockchain<A, H, R> {
	pub name: String,
	pub time: u64,
	pub accounts: HashMap<A, Amount>,
	pub events: Vec<AbstractBlockchainEvent<A, H>>,
	pub rng: R,

	pub initiater_contract: SmartContractInitiator<A, H>,
	pub counterparty_contract: SmartContractCounterparty<A, H>,

	pub transaction_sender: mpsc::UnboundedSender<Transaction<A, H>>,
	pub transaction_receiver: mpsc::UnboundedReceiver<Transaction<A, H>>,

	pub event_listeners: Vec<mpsc::UnboundedSender<AbstractBlockchainEvent<A, H>>>,

	waker: AtomicWaker,

	pub _phantom: std::marker::PhantomData<H>,
}

impl<A, H, R> AbstractBlockchain<A, H, R>
where
	A: BridgeAddressType,
	H: BridgeHashType + GenUniqueHash,
	R: RngSeededClone,
{
	pub fn new(rng: R, name: impl Into<String>) -> Self {
		let accounts = HashMap::new();
		let events = Vec::new();
		let (event_sender, event_receiver) = mpsc::unbounded();
		let event_listeners = Vec::new();

		Self {
			name: name.into(),
			time: 0,
			accounts,
			events,
			rng,
			initiater_contract: SmartContractInitiator::new(),
			counterparty_contract: SmartContractCounterparty::new(),
			transaction_sender: event_sender,
			transaction_receiver: event_receiver,
			event_listeners,
			waker: AtomicWaker::new(),
			_phantom: std::marker::PhantomData,
		}
	}

	pub fn add_event_listener(&mut self) -> mpsc::UnboundedReceiver<AbstractBlockchainEvent<A, H>> {
		let (sender, receiver) = mpsc::unbounded();
		self.event_listeners.push(sender);
		receiver
	}

	pub fn forward_time(&mut self, duration: u64) {
		self.time += duration;
	}

	pub fn add_account(&mut self, address: A, amount: Amount) {
		self.accounts.insert(address, amount);
	}

	pub fn get_balance(&mut self, address: &A) -> Option<&Amount> {
		self.accounts.get(address)
	}

	pub fn connection(&self) -> mpsc::UnboundedSender<Transaction<A, H>> {
		self.transaction_sender.clone()
	}

	pub fn client(
		&mut self,
		failure_rate: f64,
		false_positive_rate: f64,
	) -> AbstractBlockchainClient<A, H, R> {
		AbstractBlockchainClient::new(
			self.transaction_sender.clone(),
			self.rng.seeded_clone(),
			failure_rate,
			false_positive_rate,
		) // Example rates: 10% failure, 5% false positive
	}
}

impl<A, H, R> Future for AbstractBlockchain<A, H, R>
where
	A: BridgeAddressType,
	H: BridgeHashType + GenUniqueHash,
	R: Unpin,
{
	type Output = ();

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		match self.poll_next(cx) {
			Poll::Ready(None) => Poll::Ready(()),
			Poll::Pending | Poll::Ready(Some(_)) => Poll::Pending,
		}
	}
}

impl<A, H, R> Stream for AbstractBlockchain<A, H, R>
where
	A: BridgeAddressType,
	H: BridgeHashType + GenUniqueHash,
	R: Unpin,
{
	type Item = AbstractBlockchainEvent<A, H>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		tracing::trace!("AbstractBlockchain[{}]: Polling for events", self.name);
		let this = self.get_mut();

		if let Poll::Ready(Some(transaction)) = this.transaction_receiver.poll_next_unpin(cx) {
			tracing::trace!(
				"AbstractBlockchain[{}]: Received transaction: {:?}",
				this.name,
				transaction
			);
			match transaction {
				Transaction::Initiator(call) => match call {
					InitiatorCall::InitiateBridgeTransfer(
						initiator_address,
						recipient_address,
						amount,
						time_lock,
						hash_lock,
					) => {
						this.initiater_contract.initiate_bridge_transfer(
							initiator_address.clone(),
							recipient_address.clone(),
							amount.clone(),
							time_lock.clone(),
							hash_lock.clone(),
						);
						this.events.push(AbstractBlockchainEvent::BridgeTransferInitiated(
							BridgeTransferDetails {
								bridge_transfer_id: BridgeTransferId::<H>::gen_unique_hash(),
								initiator_address,
								recipient_address,
								hash_lock,
								time_lock,
								amount,
							},
						));
					}
					InitiatorCall::CompleteBridgeTransfer(bridge_transfer_id, secret) => {
						this.initiater_contract.complete_bridge_transfer(
							&mut this.accounts,
							bridge_transfer_id.clone(),
							secret.clone(),
						);
					}
				},
				Transaction::Counterparty(call) => match call {
					CounterpartyCall::LockBridgeTransfer(
						bridge_transfer_id,
						hash_lock,
						time_lock,
						recipient_address,
						amount,
					) => {
						this.counterparty_contract.lock_bridge_transfer(
							bridge_transfer_id.clone(),
							hash_lock.clone(),
							time_lock.clone(),
							recipient_address.clone(),
							amount.clone(),
						);
						this.events.push(AbstractBlockchainEvent::BridgeTransferAssetsLocked(
							LockDetails {
								bridge_transfer_id,
								hash_lock,
								time_lock,
								recipient_address,
								amount,
							},
						));
					}
				},
			}
		}

		if let Some(event) = this.events.pop() {
			for listener in &mut this.event_listeners {
				listener.unbounded_send(event.clone()).expect("listener dropped");
			}
			return Poll::Ready(Some(event));
		}

		this.waker.register(cx.waker());

		Poll::Pending
	}
}
