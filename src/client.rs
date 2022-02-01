// Copyright (C) 2021 Polytope Capital (Caymans) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Utilities for creating the neccessary client subsystems.

use crate::{
	ChainInfo, FullClientFor, Node, ParachainInherentSproofProvider,
	SharedParachainInherentProvider,
};
use futures::channel::mpsc;
use manual_seal::{
	import_queue,
	rpc::{ManualSeal, ManualSealApi},
	run_manual_seal, ConsensusDataProvider, ManualSealParams,
};
use sc_client_api::backend::Backend;
use sc_executor::NativeElseWasmExecutor;
use sc_service::{
	build_network, new_full_parts, spawn_tasks, BuildNetworkParams, Configuration,
	KeystoreContainer, SpawnTasksParams, TFullBackend,
};
use sc_transaction_pool::BasicPool;
use sp_api::{ApiExt, ConstructRuntimeApi, Core, Metadata, TransactionFor};
use sp_block_builder::BlockBuilder;
use sp_blockchain::HeaderBackend;
use sp_inherents::CreateInherentDataProviders;
use sp_offchain::OffchainWorkerApi;
use sp_runtime::traits::{Block as BlockT, Header};
use sp_session::SessionKeys;
use sp_transaction_pool::runtime_api::TaggedTransactionQueue;
use std::{
	str::FromStr,
	sync::{Arc, Mutex},
};

/// Creates all the client parts you need for [`Node`](crate::node::Node)
pub fn build_node_subsystems<T, I>(
	config: Configuration,
	is_parachain: bool,
	block_import_provider: I,
) -> Result<Node<T>, sc_service::Error>
where
	T: ChainInfo + 'static,
	<T::RuntimeApi as ConstructRuntimeApi<T::Block, FullClientFor<T>>>::RuntimeApi:
		Core<T::Block>
			+ Metadata<T::Block>
			+ OffchainWorkerApi<T::Block>
			+ SessionKeys<T::Block>
			+ TaggedTransactionQueue<T::Block>
			+ BlockBuilder<T::Block>
			+ ApiExt<T::Block, StateBackend = <TFullBackend<T::Block> as Backend<T::Block>>::State>,
	<T::Runtime as frame_system::Config>::Call: From<frame_system::Call<T::Runtime>>,
	<<T as ChainInfo>::Block as BlockT>::Hash: FromStr + Unpin,
	<<T as ChainInfo>::Block as BlockT>::Header: Unpin,
	<<<T as ChainInfo>::Block as BlockT>::Header as Header>::Number:
		num_traits::cast::AsPrimitive<usize> + num_traits::cast::AsPrimitive<u32>,
	I: Fn(
		Arc<FullClientFor<T>>,
		sc_consensus::LongestChain<TFullBackend<T::Block>, T::Block>,
		&KeystoreContainer,
		Option<SharedParachainInherentProvider<T>>,
	) -> Result<
		(
			T::BlockImport,
			Option<
				Box<
					dyn ConsensusDataProvider<
						T::Block,
						Transaction = TransactionFor<FullClientFor<T>, T::Block>,
					>,
				>,
			>,
			Box<
				dyn CreateInherentDataProviders<
					T::Block,
					(),
					InherentDataProviders = T::InherentDataProviders,
				>,
			>,
		),
		sc_service::Error,
	>,
{
	let executor = NativeElseWasmExecutor::<T::ExecutorDispatch>::new(
		config.wasm_method,
		config.default_heap_pages,
		config.max_runtime_instances,
	);

	let (client, backend, keystore, mut task_manager) =
		new_full_parts::<T::Block, T::RuntimeApi, _>(&config, None, executor)?;
	let client = Arc::new(client);

	let select_chain = sc_consensus::LongestChain::new(backend.clone());

	let parachain_inherent_provider = if is_parachain {
		Some(Arc::new(Mutex::new(ParachainInherentSproofProvider::new(client.clone()))))
	} else {
		None
	};
	let (block_import, consensus_data_provider, create_inherent_data_providers) =
		block_import_provider(
			client.clone(),
			select_chain.clone(),
			&keystore,
			parachain_inherent_provider.clone(),
		)?;
	let import_queue =
		import_queue(Box::new(block_import.clone()), &task_manager.spawn_essential_handle(), None);
	let pool = BasicPool::new_full(
		config.transaction_pool.clone(),
		true.into(),
		config.prometheus_registry(),
		task_manager.spawn_essential_handle(),
		client.clone(),
	);

	let (network, system_rpc_tx, network_starter) = {
		let params = BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			block_announce_validator_builder: None,
			warp_sync: None,
		};
		build_network(params)?
	};

	// offchain workers
	sc_service::build_offchain_workers(
		&config,
		task_manager.spawn_handle(),
		client.clone(),
		network.clone(),
	);

	// Proposer object for block authorship.
	let env = sc_basic_authorship::ProposerFactory::new(
		task_manager.spawn_handle(),
		client.clone(),
		pool.clone(),
		config.prometheus_registry(),
		None,
	);

	// Channel for the rpc handler to communicate with the authorship task.
	let (command_sink, commands_stream) = mpsc::channel(10);

	let rpc_sink = command_sink.clone();

	let rpc_handlers = {
		let params = SpawnTasksParams {
			config,
			client: client.clone(),
			backend: backend.clone(),
			task_manager: &mut task_manager,
			keystore: keystore.sync_keystore(),
			transaction_pool: pool.clone(),
			rpc_extensions_builder: Box::new(move |_, _| {
				let mut io = jsonrpc_core::IoHandler::default();
				io.extend_with(ManualSealApi::to_delegate(ManualSeal::new(rpc_sink.clone())));
				Ok(io)
			}),
			network,
			system_rpc_tx,
			telemetry: None,
		};
		spawn_tasks(params)?
	};

	// Background authorship future.
	let authorship_future = run_manual_seal(ManualSealParams {
		block_import,
		env,
		client: client.clone(),
		pool: pool.clone(),
		commands_stream,
		select_chain,
		consensus_data_provider,
		create_inherent_data_providers,
	});

	// spawn the authorship task as an essential task.
	task_manager
		.spawn_essential_handle()
		.spawn("manual-seal", None, authorship_future);

	network_starter.start_network();
	let rpc_handler = rpc_handlers.io_handler();

	let node = Node::<T> {
		rpc_handler,
		task_manager: Some(task_manager),
		pool,
		backend,
		initial_block_number: client.info().best_number,
		client,
		manual_seal_command_sink: command_sink,
		parachain_inherent_provider,
	};

	Ok(node)
}
