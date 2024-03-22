use std::collections::HashMap;
use std::io::Write;

use anyhow::{anyhow, Result};
use blockifier::execution::contract_class::ContractClass as BlockifierContractClass;
use blockifier::execution::entry_point::CallInfo;
use cairo_lang_casm_contract_class::{CasmContractClass, CasmContractEntryPoint, CasmContractEntryPoints};
use mc_sync::l1::ETHEREUM_STATE_UPDATE;
use mp_block::Block as StarknetBlock;
use mp_digest_log::find_starknet_block;
use mp_felt::Felt252Wrapper;
use mp_hashers::HasherT;
use mp_transactions::to_starknet_core_transaction::to_starknet_core_tx;
use num_bigint::BigUint;
use sp_api::{BlockT, HeaderT};
use sp_blockchain::HeaderBackend;
use starknet_api::deprecated_contract_class::{EntryPoint, EntryPointType};
use starknet_api::hash::StarkFelt;
use starknet_api::state::ThinStateDiff;
use starknet_core::types::contract::{CompiledClass, CompiledClassEntrypoint, CompiledClassEntrypointList};
use starknet_core::types::{
    BlockStatus, CompressedLegacyContractClass, ContractClass, ContractStorageDiffItem, DeclaredClassItem,
    DeployedContractItem, EntryPointsByType, Event, ExecutionResources, FieldElement, FlattenedSierraClass,
    FromByteArrayError, LegacyContractEntryPoint, LegacyEntryPointsByType, MsgToL1, NonceUpdate, ReplacedClassItem,
    ResourcePrice, StateDiff, StorageEntry, Transaction,
};

use crate::Felt;

pub fn extract_events_from_call_info(call_info: &CallInfo) -> Vec<Event> {
    let address = call_info.call.storage_address;
    let events: Vec<_> = call_info
        .execution
        .events
        .iter()
        .map(|ordered_event| Event {
            from_address: FieldElement::from_byte_slice_be(address.0.0.bytes()).unwrap(),
            keys: ordered_event
                .event
                .keys
                .iter()
                .map(|key| FieldElement::from_byte_slice_be(key.0.bytes()).unwrap())
                .collect(),
            data: ordered_event
                .event
                .data
                .0
                .iter()
                .map(|data_item| FieldElement::from_byte_slice_be(data_item.bytes()).unwrap())
                .collect(),
        })
        .collect();

    let inner_events: Vec<_> = call_info.inner_calls.iter().flat_map(extract_events_from_call_info).collect();

    events.into_iter().chain(inner_events).collect()
}

pub fn extract_messages_from_call_info(call_info: &CallInfo) -> Vec<MsgToL1> {
    let address = call_info.call.storage_address;
    let events: Vec<_> = call_info
        .execution
        .l2_to_l1_messages
        .iter()
        .map(|msg| MsgToL1 {
            from_address: FieldElement::from_byte_slice_be(address.0.0.bytes()).unwrap(),
            to_address: FieldElement::from_byte_slice_be(msg.message.to_address.0.to_fixed_bytes().as_slice()).unwrap(),
            payload: msg
                .message
                .payload
                .0
                .iter()
                .map(|data_item| FieldElement::from_byte_slice_be(data_item.bytes()).unwrap())
                .collect(),
        })
        .collect();

    let inner_messages: Vec<_> = call_info.inner_calls.iter().flat_map(extract_messages_from_call_info).collect();

    events.into_iter().chain(inner_messages).collect()
}

pub fn blockifier_call_info_to_starknet_resources(callinfo: &CallInfo) -> ExecutionResources {
    let vm_ressources = &callinfo.vm_resources;

    let steps = vm_ressources.n_steps as u64;
    let memory_holes = match vm_ressources.n_memory_holes as u64 {
        0 => None,
        n => Some(n),
    };

    let builtin_insstance = &vm_ressources.builtin_instance_counter;

    let range_check_builtin_applications = *builtin_insstance.get("range_check_builtin").unwrap_or(&0) as u64;
    let pedersen_builtin_applications = *builtin_insstance.get("pedersen_builtin").unwrap_or(&0) as u64;
    let poseidon_builtin_applications = *builtin_insstance.get("poseidon_builtin").unwrap_or(&0) as u64;
    let ec_op_builtin_applications = *builtin_insstance.get("ec_op_builtin").unwrap_or(&0) as u64;
    let ecdsa_builtin_applications = *builtin_insstance.get("ecdsa_builtin").unwrap_or(&0) as u64;
    let bitwise_builtin_applications = *builtin_insstance.get("bitwise_builtin").unwrap_or(&0) as u64;
    let keccak_builtin_applications = *builtin_insstance.get("keccak_builtin").unwrap_or(&0) as u64;

    ExecutionResources {
        steps,
        memory_holes,
        range_check_builtin_applications,
        pedersen_builtin_applications,
        poseidon_builtin_applications,
        ec_op_builtin_applications,
        ecdsa_builtin_applications,
        bitwise_builtin_applications,
        keccak_builtin_applications,
    }
}

#[allow(dead_code)]
pub fn blockifier_to_starknet_rs_ordered_events(
    ordered_events: &[blockifier::execution::entry_point::OrderedEvent],
) -> Vec<starknet_core::types::OrderedEvent> {
    ordered_events
        .iter()
        .map(|event| starknet_core::types::OrderedEvent {
            order: event.order as u64, // Convert usize to u64
            keys: event.event.keys.iter().map(|key| FieldElement::from_byte_slice_be(key.0.bytes()).unwrap()).collect(),
            data: event
                .event
                .data
                .0
                .iter()
                .map(|data_item| FieldElement::from_byte_slice_be(data_item.bytes()).unwrap())
                .collect(),
        })
        .collect()
}

pub(crate) fn tx_hash_retrieve(tx_hashes: Vec<StarkFelt>) -> Vec<FieldElement> {
    let mut v = Vec::with_capacity(tx_hashes.len());
    for tx_hash in tx_hashes {
        v.push(FieldElement::from(Felt252Wrapper::from(tx_hash)));
    }
    v
}

pub(crate) fn tx_hash_compute<H>(block: &mp_block::Block, chain_id: Felt) -> Vec<FieldElement>
where
    H: HasherT + Send + Sync + 'static,
{
    block
        .transactions_hashes::<H>(chain_id.0.into(), Some(block.header().block_number))
        .map(FieldElement::from)
        .collect()
}

pub(crate) fn tx_conv(txs: &[mp_transactions::Transaction], tx_hashes: Vec<FieldElement>) -> Vec<Transaction> {
    txs.iter().zip(tx_hashes).map(|(tx, hash)| to_starknet_core_tx(tx.clone(), hash)).collect()
}

pub(crate) fn status(block_number: u64) -> BlockStatus {
    if block_number <= ETHEREUM_STATE_UPDATE.read().unwrap().block_number {
        BlockStatus::AcceptedOnL1
    } else {
        BlockStatus::AcceptedOnL2
    }
}

pub(crate) fn parent_hash(block: &mp_block::Block) -> FieldElement {
    Felt252Wrapper::from(block.header().parent_block_hash).into()
}

pub(crate) fn new_root(block: &mp_block::Block) -> FieldElement {
    Felt252Wrapper::from(block.header().global_state_root).into()
}

pub(crate) fn timestamp(block: &mp_block::Block) -> u64 {
    block.header().block_timestamp
}

pub(crate) fn sequencer_address(block: &mp_block::Block) -> FieldElement {
    Felt252Wrapper::from(block.header().sequencer_address).into()
}

pub(crate) fn l1_gas_price(block: &mp_block::Block) -> ResourcePrice {
    block.header().l1_gas_price.into()
}

pub(crate) fn starknet_version(block: &mp_block::Block) -> String {
    block.header().protocol_version.from_utf8().expect("starknet version should be a valid utf8 string")
}

/// Returns a [`ContractClass`] from a [`BlockifierContractClass`]
pub fn to_rpc_contract_class(contract_class: BlockifierContractClass) -> Result<ContractClass> {
    match contract_class {
        BlockifierContractClass::V0(contract_class) => {
            let entry_points_by_type = to_legacy_entry_points_by_type(&contract_class.entry_points_by_type)?;
            let compressed_program = compress(&contract_class.program.to_bytes())?;
            Ok(ContractClass::Legacy(CompressedLegacyContractClass {
                program: compressed_program,
                entry_points_by_type,
                // FIXME 723
                abi: None,
            }))
        }
        BlockifierContractClass::V1(_contract_class) => Ok(ContractClass::Sierra(FlattenedSierraClass {
            sierra_program: vec![], // FIXME: https://github.com/keep-starknet-strange/madara/issues/775
            contract_class_version: option_env!("COMPILER_VERSION").unwrap_or("0.11.2").into(),
            entry_points_by_type: EntryPointsByType { constructor: vec![], external: vec![], l1_handler: vec![] }, /* TODO: add entry_points_by_type */
            abi: String::from("{}"), // FIXME: https://github.com/keep-starknet-strange/madara/issues/790
        })),
    }
}

/// Returns a [`StateDiff`] from a [`ThinStateDiff`]
pub fn to_rpc_state_diff(thin_state_diff: ThinStateDiff) -> StateDiff {
    let nonces = thin_state_diff
        .nonces
        .iter()
        .map(|x| NonceUpdate {
            contract_address: Felt252Wrapper::from(x.0.0.0).into(),
            nonce: Felt252Wrapper::from(x.1.0).into(),
        })
        .collect();

    let storage_diffs = thin_state_diff
        .storage_diffs
        .iter()
        .map(|x| ContractStorageDiffItem {
            address: Felt252Wrapper::from(x.0.0.0).into(),
            storage_entries: x
                .1
                .iter()
                .map(|y| StorageEntry {
                    key: Felt252Wrapper::from(y.0.0.0).into(),
                    value: Felt252Wrapper::from(*y.1).into(),
                })
                .collect(),
        })
        .collect();

    let deprecated_declared_classes =
        thin_state_diff.deprecated_declared_classes.iter().map(|x| Felt252Wrapper::from(x.0).into()).collect();

    let declared_classes = thin_state_diff
        .declared_classes
        .iter()
        .map(|x| DeclaredClassItem {
            class_hash: Felt252Wrapper::from(x.0.0).into(),
            compiled_class_hash: Felt252Wrapper::from(x.1.0).into(),
        })
        .collect();

    let deployed_contracts = thin_state_diff
        .deployed_contracts
        .iter()
        .map(|x| DeployedContractItem {
            address: Felt252Wrapper::from(x.0.0.0).into(),
            class_hash: Felt252Wrapper::from(x.1.0).into(),
        })
        .collect();

    let replaced_classes = thin_state_diff
        .replaced_classes
        .iter()
        .map(|x| ReplacedClassItem {
            contract_address: Felt252Wrapper::from(x.0.0.0).into(),
            class_hash: Felt252Wrapper::from(x.1.0).into(),
        })
        .collect();

    StateDiff {
        nonces,
        storage_diffs,
        deprecated_declared_classes,
        declared_classes,
        deployed_contracts,
        replaced_classes,
    }
}

/// Returns a compressed vector of bytes
pub(crate) fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut gzip_encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    // 2023-08-22: JSON serialization is already done in Blockifier
    // https://github.com/keep-starknet-strange/blockifier/blob/no_std-support-7578442/crates/blockifier/src/execution/contract_class.rs#L129
    // https://github.com/keep-starknet-strange/blockifier/blob/no_std-support-7578442/crates/blockifier/src/execution/contract_class.rs#L389
    // serde_json::to_writer(&mut gzip_encoder, data)?;
    gzip_encoder.write_all(data)?;
    Ok(gzip_encoder.finish()?)
}

/// Returns a [Result<LegacyEntryPointsByType>] (starknet-rs type) from a [HashMap<EntryPointType,
/// Vec<EntryPoint>>]
pub fn to_legacy_entry_points_by_type(
    entries: &HashMap<EntryPointType, Vec<EntryPoint>>,
) -> Result<LegacyEntryPointsByType> {
    fn collect_entry_points(
        entries: &HashMap<EntryPointType, Vec<EntryPoint>>,
        entry_point_type: EntryPointType,
    ) -> Result<Vec<LegacyContractEntryPoint>> {
        Ok(entries
            .get(&entry_point_type)
            .ok_or(anyhow!("Missing {:?} entry point", entry_point_type))?
            .iter()
            .map(|e| to_legacy_entry_point(e.clone()))
            .collect::<Result<Vec<LegacyContractEntryPoint>, FromByteArrayError>>()?)
    }

    let constructor = collect_entry_points(entries, EntryPointType::Constructor)?;
    let external = collect_entry_points(entries, EntryPointType::External)?;
    let l1_handler = collect_entry_points(entries, EntryPointType::L1Handler)?;

    Ok(LegacyEntryPointsByType { constructor, external, l1_handler })
}

/// Returns a [LegacyContractEntryPoint] (starknet-rs) from a [EntryPoint] (starknet-api)
pub fn to_legacy_entry_point(entry_point: EntryPoint) -> Result<LegacyContractEntryPoint, FromByteArrayError> {
    let selector = FieldElement::from_bytes_be(&entry_point.selector.0.0)?;
    let offset = entry_point.offset.0 as u64;
    Ok(LegacyContractEntryPoint { selector, offset })
}

/// Returns the current Starknet block from the block header's digest
pub fn get_block_by_block_hash<B, C>(client: &C, block_hash: <B as BlockT>::Hash) -> Result<StarknetBlock>
where
    B: BlockT,
    C: HeaderBackend<B>,
{
    let header =
        client.header(block_hash).ok().flatten().ok_or_else(|| anyhow::Error::msg("Failed to retrieve header"))?;
    let digest = header.digest();
    let block = find_starknet_block(digest)?;
    Ok(block)
}

// Utils to convert Casm contract class to Compiled class
pub fn get_casm_cotract_class_hash(casm_contract_class: &CasmContractClass) -> FieldElement {
    let compiled_class = casm_contract_class_to_compiled_class(casm_contract_class);
    compiled_class.class_hash().unwrap()
}

/// Converts a [CasmContractClass] to a [CompiledClass]
pub fn casm_contract_class_to_compiled_class(casm_contract_class: &CasmContractClass) -> CompiledClass {
    CompiledClass {
        prime: casm_contract_class.prime.to_string(),
        compiler_version: casm_contract_class.compiler_version.clone(),
        bytecode: casm_contract_class.bytecode.iter().map(|x| biguint_to_field_element(&x.value)).collect(),
        entry_points_by_type: casm_entry_points_to_compiled_entry_points(&casm_contract_class.entry_points_by_type),
        hints: vec![],        // not needed to get class hash so ignoring this
        pythonic_hints: None, // not needed to get class hash so ignoring this
    }
}

/// Converts a [CasmContractEntryPoints] to a [CompiledClassEntrypointList]
pub fn casm_entry_points_to_compiled_entry_points(value: &CasmContractEntryPoints) -> CompiledClassEntrypointList {
    CompiledClassEntrypointList {
        external: value.external.iter().map(casm_entry_point_to_compiled_entry_point).collect(),
        l1_handler: value.l1_handler.iter().map(casm_entry_point_to_compiled_entry_point).collect(),
        constructor: value.constructor.iter().map(casm_entry_point_to_compiled_entry_point).collect(),
    }
}

/// Converts a [CasmContractEntryPoint] to a [CompiledClassEntrypoint]
pub fn casm_entry_point_to_compiled_entry_point(value: &CasmContractEntryPoint) -> CompiledClassEntrypoint {
    CompiledClassEntrypoint {
        selector: biguint_to_field_element(&value.selector),
        offset: value.offset.try_into().unwrap(),
        builtins: value.builtins.clone(),
    }
}

/// Converts a [BigUint] to a [FieldElement]
pub fn biguint_to_field_element(value: &BigUint) -> FieldElement {
    let bytes = value.to_bytes_be();
    FieldElement::from_byte_slice_be(bytes.as_slice()).unwrap()
}
