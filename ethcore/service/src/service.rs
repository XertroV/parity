// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Creates and registers client and network services.

use std::sync::Arc;
use std::path::Path;

use ansi_term::Colour;
use io::{IoContext, TimerToken, IoHandler, IoService, IoError};
use kvdb::KeyValueDB;
use kvdb_rocksdb::{Database, DatabaseConfig};
use stop_guard::StopGuard;

use ethcore::client::{Client, ClientConfig, ChainNotify, ClientIoMessage};
use ethcore::{db, error};
use ethcore::miner::Miner;
use ethcore::snapshot::service::{Service as SnapshotService, ServiceParams as SnapServiceParams};
use ethcore::snapshot::{RestorationStatus};
use ethcore::spec::Spec;
use ethcore::account_provider::AccountProvider;

use private_transactions;
use Error;

/// Client service setup. Creates and registers client and network services with the IO subsystem.
pub struct ClientService {
	io_service: Arc<IoService<ClientIoMessage>>,
	client: Arc<Client>,
	snapshot: Arc<SnapshotService>,
	private_tx: Arc<private_transactions::Provider>,
	database: Arc<Database>,
	_stop_guard: StopGuard,
}

impl ClientService {
	/// Start the `ClientService`.
	pub fn start(
		config: ClientConfig,
		spec: &Spec,
		client_path: &Path,
		snapshot_path: &Path,
		_ipc_path: &Path,
		miner: Arc<Miner>,
		account_provider: Arc<AccountProvider>,
		encryptor: Box<private_transactions::Encryptor>,
		private_tx_conf: private_transactions::ProviderConfig,
		) -> Result<ClientService, Error>
	{
		let io_service = IoService::<ClientIoMessage>::start()?;

		info!("Configured for {} using {} engine", Colour::White.bold().paint(spec.name.clone()), Colour::Yellow.bold().paint(spec.engine.name()));

		let mut db_config = DatabaseConfig::with_columns(db::NUM_COLUMNS);

		db_config.memory_budget = config.db_cache_size;
		db_config.compaction = config.db_compaction.compaction_profile(client_path);
		db_config.wal = config.db_wal;

		let db = Arc::new(Database::open(
			&db_config,
			&client_path.to_str().expect("DB path could not be converted to string.")
		).map_err(error::Error::Database)?);


		let pruning = config.pruning;
		let client = Client::new(config, &spec, db.clone(), miner, io_service.channel())?;

		let snapshot_params = SnapServiceParams {
			engine: spec.engine.clone(),
			genesis_block: spec.genesis_block(),
			db_config: db_config.clone(),
			pruning: pruning,
			channel: io_service.channel(),
			snapshot_root: snapshot_path.into(),
			db_restore: client.clone(),
		};
		let snapshot = Arc::new(SnapshotService::new(snapshot_params)?);

		let private_tx = Arc::new(private_transactions::Provider::new(client.clone(), account_provider, encryptor, private_tx_conf, io_service.channel())?);

		let client_io = Arc::new(ClientIoHandler {
			client: client.clone(),
			snapshot: snapshot.clone(),
			private_tx: private_tx.clone(),
		});
		io_service.register_handler(client_io)?;

		spec.engine.register_client(Arc::downgrade(&client) as _);

		let stop_guard = StopGuard::new();

		Ok(ClientService {
			io_service: Arc::new(io_service),
			client: client,
			snapshot: snapshot,
			private_tx,
			database: db,
			_stop_guard: stop_guard,
		})
	}

	/// Get general IO interface
	pub fn register_io_handler(&self, handler: Arc<IoHandler<ClientIoMessage> + Send>) -> Result<(), IoError> {
		self.io_service.register_handler(handler)
	}

	/// Get client interface
	pub fn client(&self) -> Arc<Client> {
		self.client.clone()
	}

	/// Get snapshot interface.
	pub fn snapshot_service(&self) -> Arc<SnapshotService> {
		self.snapshot.clone()
	}

	/// Get private transaction service.
	pub fn private_tx_service(&self) -> Arc<private_transactions::Provider> {
		self.private_tx.clone()
	}

	/// Get network service component
	pub fn io(&self) -> Arc<IoService<ClientIoMessage>> {
		self.io_service.clone()
	}

	/// Set the actor to be notified on certain chain events
	pub fn add_notify(&self, notify: Arc<ChainNotify>) {
		self.client.add_notify(notify);
	}

	/// Get a handle to the database.
	pub fn db(&self) -> Arc<KeyValueDB> { self.database.clone() }
}

/// IO interface for the Client handler
struct ClientIoHandler {
	client: Arc<Client>,
	snapshot: Arc<SnapshotService>,
	private_tx: Arc<private_transactions::Provider>,
}

const CLIENT_TICK_TIMER: TimerToken = 0;
const SNAPSHOT_TICK_TIMER: TimerToken = 1;

const CLIENT_TICK_MS: u64 = 5000;
const SNAPSHOT_TICK_MS: u64 = 10000;

impl IoHandler<ClientIoMessage> for ClientIoHandler {
	fn initialize(&self, io: &IoContext<ClientIoMessage>) {
		io.register_timer(CLIENT_TICK_TIMER, CLIENT_TICK_MS).expect("Error registering client timer");
		io.register_timer(SNAPSHOT_TICK_TIMER, SNAPSHOT_TICK_MS).expect("Error registering snapshot timer");
	}

	fn timeout(&self, _io: &IoContext<ClientIoMessage>, timer: TimerToken) {
		match timer {
			CLIENT_TICK_TIMER => {
				use ethcore::snapshot::SnapshotService;
				let snapshot_restoration = if let RestorationStatus::Ongoing{..} = self.snapshot.status() { true } else { false };
				self.client.tick(snapshot_restoration)
			},
			SNAPSHOT_TICK_TIMER => self.snapshot.tick(),
			_ => warn!("IO service triggered unregistered timer '{}'", timer),
		}
	}

	fn message(&self, _io: &IoContext<ClientIoMessage>, net_message: &ClientIoMessage) {
		use std::thread;

		match *net_message {
			ClientIoMessage::BlockVerified => { self.client.import_verified_blocks(); }
			ClientIoMessage::NewTransactions(ref transactions, peer_id) => {
				self.client.import_queued_transactions(transactions, peer_id);
			}
			ClientIoMessage::BeginRestoration(ref manifest) => {
				if let Err(e) = self.snapshot.init_restore(manifest.clone(), true) {
					warn!("Failed to initialize snapshot restoration: {}", e);
				}
			}
			ClientIoMessage::FeedStateChunk(ref hash, ref chunk) => self.snapshot.feed_state_chunk(*hash, chunk),
			ClientIoMessage::FeedBlockChunk(ref hash, ref chunk) => self.snapshot.feed_block_chunk(*hash, chunk),
			ClientIoMessage::TakeSnapshot(num) => {
				let client = self.client.clone();
				let snapshot = self.snapshot.clone();

				let res = thread::Builder::new().name("Periodic Snapshot".into()).spawn(move || {
					if let Err(e) = snapshot.take_snapshot(&*client, num) {
						warn!("Failed to take snapshot at block #{}: {}", num, e);
					}
				});

				if let Err(e) = res {
					debug!(target: "snapshot", "Failed to initialize periodic snapshot thread: {:?}", e);
				}
			},
			ClientIoMessage::NewMessage(ref message) => if let Err(e) = self.client.engine().handle_message(message) {
				trace!(target: "poa", "Invalid message received: {}", e);
			},
			ClientIoMessage::NewPrivateTransaction => if let Err(e) = self.private_tx.on_private_transaction_queued() {
				warn!("Failed to handle private transaction {:?}", e);
			},
			_ => {} // ignore other messages
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::{time, thread};

	use tempdir::TempDir;

	use ethcore::account_provider::AccountProvider;
	use ethcore::client::ClientConfig;
	use ethcore::miner::Miner;
	use ethcore::spec::Spec;
	use super::*;

	use private_transactions;

	#[test]
	fn it_can_be_started() {
		let tempdir = TempDir::new("").unwrap();
		let client_path = tempdir.path().join("client");
		let snapshot_path = tempdir.path().join("snapshot");

		let spec = Spec::new_test();
		let service = ClientService::start(
			ClientConfig::default(),
			&spec,
			&client_path,
			&snapshot_path,
			tempdir.path(),
			Arc::new(Miner::with_spec(&spec)),
			Arc::new(AccountProvider::transient_provider()),
			Box::new(private_transactions::SecretStoreEncryptor::new(Default::default()).unwrap()),
			Default::default()
		);
		assert!(service.is_ok());
		drop(service.unwrap());
		thread::park_timeout(time::Duration::from_millis(100));
	}
}
