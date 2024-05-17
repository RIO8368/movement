use mempool_util::{MempoolBlockOperations, MempoolTransactionOperations};
pub use move_rocks::RocksdbMempool;
pub use movement_types::{Block, Id, Transaction};
pub use sequencing_util::Sequencer;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct Memseq<T: MempoolBlockOperations + MempoolTransactionOperations> {
	pub mempool: Arc<RwLock<T>>,
	// this value should not be changed after initialization
	block_size: u32,
	pub parent_block: Arc<RwLock<Id>>,
	// this value should not be changed after initialization
	building_time_ms: u64,
}

impl<T: MempoolBlockOperations + MempoolTransactionOperations> Memseq<T> {
	pub fn new(
		mempool: Arc<RwLock<T>>,
		block_size: u32,
		parent_block: Arc<RwLock<Id>>,
		building_time_ms: u64,
	) -> Self {
		Self { mempool, block_size, parent_block, building_time_ms }
	}

	pub fn with_block_size(mut self, block_size: u32) -> Self {
		self.block_size = block_size;
		self
	}

	pub fn with_building_time_ms(mut self, building_time_ms: u64) -> Self {
		self.building_time_ms = building_time_ms;
		self
	}
}

impl Memseq<RocksdbMempool> {
	pub fn try_move_rocks(path: PathBuf) -> Result<Self, anyhow::Error> {
		let mempool = RocksdbMempool::try_new(
			path.to_str().ok_or(anyhow::anyhow!("PathBuf to str failed"))?,
		)?;
		let mempool = Arc::new(RwLock::new(mempool));
		let parent_block = Arc::new(RwLock::new(Id::default()));
		Ok(Self::new(mempool, 10, parent_block, 1000))
	}

	pub fn try_move_rocks_from_env() -> Result<Self, anyhow::Error> {
		let path = std::env::var("MOVE_ROCKS_PATH")
			.or(Err(anyhow::anyhow!("MOVE_ROCKS_PATH not found")))?;
		Self::try_move_rocks(PathBuf::from(path))
	}
}

impl<T: MempoolBlockOperations + MempoolTransactionOperations> Sequencer for Memseq<T> {
	async fn publish(&self, transaction: Transaction) -> Result<(), anyhow::Error> {
		let mempool = self.mempool.read().await;
		mempool.add_transaction(transaction).await?;
		Ok(())
	}

	async fn wait_for_next_block(&self) -> Result<Option<Block>, anyhow::Error> {
		let mempool = self.mempool.read().await;
		let mut transactions = Vec::new();

		let mut now = std::time::Instant::now();
		let finish_by = now + std::time::Duration::from_millis(self.building_time_ms);

		loop {
			let current_block_size = transactions.len() as u32;
			if current_block_size >= self.block_size {
				break;
			}

			for _ in 0..self.block_size - current_block_size {
				if let Some(transaction) = mempool.pop_transaction().await? {
					transactions.push(transaction);
				} else {
					break;
				}
			}

			// sleep to yield to other tasks and wait for more transactions
			tokio::time::sleep(std::time::Duration::from_millis(1)).await;

			now = std::time::Instant::now();
			if now > finish_by {
				break;
			}
		}

		if transactions.is_empty() {
			Ok(None)
		} else {
			Ok(Some(Block::new(
				Default::default(),
				self.parent_block.read().await.clone().to_vec(),
				transactions,
			)))
		}
	}
}

#[cfg(test)]
pub mod test {

	use super::*;
	use tempfile::tempdir;

	#[tokio::test]
	async fn test_try_move_rocks() -> Result<(), anyhow::Error> {
		let dir = tempdir()?;
		let path = dir.path().to_path_buf();
		let memseq = Memseq::try_move_rocks(path.clone())?;

		assert_eq!(memseq.block_size, 10);
		assert_eq!(memseq.building_time_ms, 1000);

		// Test invalid path
		let invalid_path = PathBuf::from("");
		let result = Memseq::try_move_rocks(invalid_path);
		assert!(result.is_err());

		Ok(())
	}

	#[tokio::test]
	async fn test_try_move_rocks_from_env() -> Result<(), anyhow::Error> {
		let dir = tempdir()?;
		let path = dir.path().to_path_buf();
		std::env::set_var("MOVE_ROCKS_PATH", path.to_str().unwrap());

		let memseq = Memseq::try_move_rocks_from_env()?;
		assert_eq!(memseq.block_size, 10);
		assert_eq!(memseq.building_time_ms, 1000);

		// Test environment variable not set
		std::env::remove_var("MOVE_ROCKS_PATH");
		let result = Memseq::try_move_rocks_from_env();
		assert!(result.is_err());

		Ok(())
	}

	#[tokio::test]
	async fn test_memseq_initialization() -> Result<(), anyhow::Error> {
		let dir = tempdir()?;
		let path = dir.path().to_path_buf();

		let mem_pool = Arc::new(RwLock::new(RocksdbMempool::try_new(
			path.to_str().ok_or(anyhow::anyhow!("PathBuf to str failed"))?,
		)?));
		let block_size = 50;
		let building_time_ms = 2000;
		let parent_block = Arc::new(RwLock::new(Id::default()));

		let memseq = Memseq::new(mem_pool, block_size, Arc::clone(&parent_block), building_time_ms);

		assert_eq!(memseq.block_size, block_size);
		assert_eq!(memseq.building_time_ms, building_time_ms);
		assert_eq!(*memseq.parent_block.read().await, *parent_block.read().await);

		Ok(())
	}

	#[tokio::test]
	async fn test_memseq_with_methods() -> Result<(), anyhow::Error> {
		let dir = tempdir()?;
		let path = dir.path().to_path_buf();

		let mem_pool = Arc::new(RwLock::new(RocksdbMempool::try_new(
			path.to_str().ok_or(anyhow::anyhow!("PathBuf to str failed"))?,
		)?));
		let block_size = 50;
		let building_time_ms = 2000;
		let parent_block = Arc::new(RwLock::new(Id::default()));

		let memseq = Memseq::new(mem_pool, block_size, Arc::clone(&parent_block), building_time_ms);

		// Test with_block_size
		let new_block_size = 100;
		let memseq = memseq.with_block_size(new_block_size);
		assert_eq!(memseq.block_size, new_block_size);

		// Test with_building_time_ms
		let new_building_time_ms = 5000;
		let memseq = memseq.with_building_time_ms(new_building_time_ms);
		assert_eq!(memseq.building_time_ms, new_building_time_ms);

		Ok(())
	}

	#[tokio::test]
	async fn test_memseq() -> Result<(), anyhow::Error> {
		let dir = tempdir()?;
		let path = dir.path().to_path_buf();
		let memseq = Memseq::try_move_rocks(path)?;

		let transaction = Transaction::new(vec![1, 2, 3]);
		memseq.publish(transaction.clone()).await?;

		let block = memseq.wait_for_next_block().await?;

		assert_eq!(block.ok_or(anyhow::anyhow!("Block not found"))?.transactions[0], transaction);

		Ok(())
	}

	#[tokio::test]
	async fn test_respects_size() -> Result<(), anyhow::Error> {
		let dir = tempdir()?;
		let path = dir.path().to_path_buf();
		let block_size = 100;
		let memseq = Memseq::try_move_rocks(path)?.with_block_size(block_size);

		let mut transactions = Vec::new();
		for i in 0..block_size * 2 {
			let transaction = Transaction::new(vec![i as u8]);
			memseq.publish(transaction.clone()).await?;
			transactions.push(transaction);
		}

		let block = memseq.wait_for_next_block().await?;

		assert!(block.is_some());

		let block = block.ok_or(anyhow::anyhow!("Block not found"))?;

		assert_eq!(block.transactions.len(), block_size as usize);

		let second_block = memseq.wait_for_next_block().await?;

		assert!(second_block.is_some());

		let second_block = second_block.ok_or(anyhow::anyhow!("Second block not found"))?;

		assert_eq!(second_block.transactions.len(), block_size as usize);

		Ok(())
	}

	#[tokio::test]
	async fn test_respects_time() -> Result<(), anyhow::Error> {
		let dir = tempdir()?;
		let path = dir.path().to_path_buf();
		let block_size = 100;
		let memseq = Memseq::try_move_rocks(path)?
			.with_block_size(block_size)
			.with_building_time_ms(500);

		let building_memseq = Arc::new(memseq);
		let waiting_memseq = Arc::clone(&building_memseq);

		let building_task = async move {
			let memseq = building_memseq;

			// add half of the transactions
			for i in 0..block_size / 2 {
				let transaction = Transaction::new(vec![i as u8]);
				memseq.publish(transaction.clone()).await?;
			}

			tokio::time::sleep(std::time::Duration::from_millis(600)).await;

			// add the rest of the transactions
			for i in block_size / 2..block_size - 2 {
				let transaction = Transaction::new(vec![i as u8]);
				memseq.publish(transaction.clone()).await?;
			}

			Ok::<_, anyhow::Error>(())
		};

		let waiting_task = async move {
			let memseq = waiting_memseq;

			// first block
			let block = memseq.wait_for_next_block().await?;
			assert!(block.is_some());
			let block = block.ok_or(anyhow::anyhow!("Block not found"))?;
			assert_eq!(block.transactions.len(), (block_size / 2) as usize);

			tokio::time::sleep(std::time::Duration::from_millis(200)).await;

			// second block
			let block = memseq.wait_for_next_block().await?;
			assert!(block.is_some());
			let block = block.ok_or(anyhow::anyhow!("Block not found"))?;
			assert_eq!(block.transactions.len(), ((block_size / 2) - 2) as usize);

			Ok::<_, anyhow::Error>(())
		};

		tokio::try_join!(building_task, waiting_task)?;

		Ok(())
	}
}
