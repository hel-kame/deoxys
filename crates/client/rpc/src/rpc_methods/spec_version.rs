use jsonrpsee::core::{async_trait, RpcResult};
use mc_genesis_data_provider::GenesisProvider;
pub use mc_rpc_core::utils::*;
use mc_rpc_core::SpecVersionServer;
pub use mc_rpc_core::{Felt, StarknetReadRpcApiServer, StarknetTraceRpcApiServer, StarknetWriteRpcApiServer};
use mp_hashers::HasherT;
use pallet_starknet_runtime_api::{ConvertTransactionRuntimeApi, StarknetRuntimeApi};
use sc_client_api::backend::{Backend, StorageProvider};
use sc_client_api::BlockBackend;
use sc_transaction_pool_api::TransactionPool;
use sc_transaction_pool::ChainApi;
use sp_api::ProvideRuntimeApi;
use sp_blockchain::HeaderBackend;
use sp_runtime::traits::Block as BlockT;
pub use mc_rpc_core::BlockNumberServer;
use crate::Starknet;

#[async_trait]
#[allow(unused_variables)]
impl<A, B, BE, G, C, P, H> SpecVersionServer for Starknet<A, B, BE, G, C, P, H>
where
    A: ChainApi<Block = B> + 'static,
    B: BlockT,
    P: TransactionPool<Block = B> + 'static,
    BE: Backend<B> + 'static,
    C: HeaderBackend<B> + BlockBackend<B> + StorageProvider<B, BE> + 'static,
    C: ProvideRuntimeApi<B>,
    C::Api: StarknetRuntimeApi<B> + ConvertTransactionRuntimeApi<B>,
    G: GenesisProvider + Send + Sync + 'static,
    H: HasherT + Send + Sync + 'static,
{
    /// Returns the Version of the StarkNet JSON-RPC Specification Being Used
    ///
    /// This method provides the version of the StarkNet JSON-RPC specification that the node is
    /// currently using. The version is returned as a semantic versioning (SemVer) string.
    ///
    /// # Arguments
    ///
    /// This method does not take any arguments.
    ///
    /// # Returns
    ///
    /// * `spec_version` - A string representing the SemVer of the StarkNet JSON-RPC specification
    ///   being used.
    fn spec_version(&self) -> RpcResult<String> {
        self.current_spec_version()
    }
}