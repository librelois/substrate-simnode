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
#![deny(unused_extern_crates, missing_docs)]

//! Basic example of end to end runtime tests.

use grandpa::GrandpaBlockImport;
use sc_consensus_babe::BabeBlockImport;
use sc_consensus_manual_seal::consensus::timestamp::SlotTimestampProvider;
use sc_service::TFullBackend;
use sp_runtime::generic::Era;
use substrate_simnode::{ChainInfo, FullClientFor, SignatureVerificationOverride};

type BlockImport<B, BE, C, SC> = BabeBlockImport<B, C, GrandpaBlockImport<BE, B, C, SC>>;

/// A unit struct which implements `NativeExecutionDispatch` feeding in the
/// hard-coded runtime.
pub struct ExecutorDispatch;

impl sc_executor::NativeExecutionDispatch for ExecutorDispatch {
	type ExtendHostFunctions =
		(frame_benchmarking::benchmarking::HostFunctions, SignatureVerificationOverride);

	fn dispatch(method: &str, data: &[u8]) -> Option<Vec<u8>> {
		node_runtime::api::dispatch(method, data)
	}

	fn native_version() -> sc_executor::NativeVersion {
		node_runtime::native_version()
	}
}

/// ChainInfo implementation.
struct NodeTemplateChainInfo;

impl ChainInfo for NodeTemplateChainInfo {
	type Block = node_primitives::Block;
	type ExecutorDispatch = ExecutorDispatch;
	type Runtime = node_runtime::Runtime;
	type RuntimeApi = node_runtime::RuntimeApi;
	type SelectChain = sc_consensus::LongestChain<TFullBackend<Self::Block>, Self::Block>;
	type BlockImport =
		BlockImport<Self::Block, TFullBackend<Self::Block>, FullClientFor<Self>, Self::SelectChain>;
	type SignedExtras = node_runtime::SignedExtra;
	type InherentDataProviders =
		(SlotTimestampProvider, sp_consensus_babe::inherents::InherentDataProvider);

	fn signed_extras(
		from: <Self::Runtime as frame_system::Config>::AccountId,
	) -> Self::SignedExtras {
		(
			frame_system::CheckSpecVersion::<Self::Runtime>::new(),
			frame_system::CheckTxVersion::<Self::Runtime>::new(),
			frame_system::CheckGenesis::<Self::Runtime>::new(),
			frame_system::CheckEra::<Self::Runtime>::from(Era::Immortal),
			frame_system::CheckNonce::<Self::Runtime>::from(
				frame_system::Pallet::<Self::Runtime>::account_nonce(from),
			),
			frame_system::CheckWeight::<Self::Runtime>::new(),
			pallet_asset_tx_payment::ChargeAssetTxPayment::<Self::Runtime>::from(0, None),
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use node_cli::chain_spec::development_config;
	use sc_consensus_manual_seal::consensus::babe::BabeConsensusDataProvider;
	use sp_consensus_babe::AuthorityId;
	use sp_keyring::sr25519::Keyring::Alice;
	use sp_runtime::{traits::IdentifyAccount, MultiSigner};
	use std::sync::Arc;
	use substrate_simnode::{build_node_subsystems, build_runtime, ConfigOrChainSpec};

	#[test]
	fn substrate_simnode() {
		let tokio_runtime = build_runtime().unwrap();
		let node = build_node_subsystems::<NodeTemplateChainInfo, _>(
			ConfigOrChainSpec::ChainSpec(
				Box::new(development_config()),
				tokio_runtime.handle().clone(),
			),
			|client, select_chain, keystore| {
				let (grandpa_block_import, ..) = grandpa::block_import(
					client.clone(),
					&(client.clone() as Arc<_>),
					select_chain.clone(),
					None,
				)?;

				let slot_duration = sc_consensus_babe::Config::get_or_compute(&*client)?;
				let (block_import, babe_link) = sc_consensus_babe::block_import(
					slot_duration.clone(),
					grandpa_block_import,
					client.clone(),
				)?;

				let consensus_data_provider = BabeConsensusDataProvider::new(
					client.clone(),
					keystore.sync_keystore(),
					babe_link.epoch_changes().clone(),
					vec![(AuthorityId::from(Alice.public()), 1000)],
				)
				.expect("failed to create ConsensusDataProvider");

				let cloned_client = client.clone();
				let create_inherent_data_providers = Box::new(move |_, _| {
					let client = cloned_client.clone();
					async move {
						let timestamp = SlotTimestampProvider::babe(client.clone())
							.map_err(|err| format!("{:?}", err))?;
						let babe = sp_consensus_babe::inherents::InherentDataProvider::new(
							timestamp.slot().into(),
						);
						Ok((timestamp, babe))
					}
				});

				Ok((
					block_import,
					Some(Box::new(consensus_data_provider)),
					create_inherent_data_providers,
				))
			},
		)
		.unwrap();

		tokio_runtime.block_on(async {
			// seals blocks
			node.seal_blocks(1).await;
			// submit extrinsics
			let alice = MultiSigner::from(Alice.public()).into_account();
			let _hash = node
				.submit_extrinsic(
					frame_system::Call::remark { remark: (b"hello world").to_vec() },
					Some(alice),
				)
				.await
				.unwrap();

			// look ma, I can read state.
			let _events =
				node.with_state(|| frame_system::Pallet::<node_runtime::Runtime>::events());
			// get access to the underlying client.
			let _client = node.client();
		})
	}
}
