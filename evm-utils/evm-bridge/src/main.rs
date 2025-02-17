mod pool;
mod sol_proxy;

use log::*;
use solana_sdk::commitment_config::CommitmentConfig;
use std::future::ready;
use std::str::FromStr;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

use evm_rpc::bridge::BridgeERPC;
use evm_rpc::chain::ChainERPC;
use evm_rpc::general::GeneralERPC;
use evm_rpc::trace::TraceERPC;
use evm_rpc::error::{Error, *};
use evm_rpc::trace::TraceMeta;
use evm_rpc::*;
use evm_state::*;
use sha3::{Digest, Keccak256};

use jsonrpc_core::BoxFuture;
use jsonrpc_http_server::jsonrpc_core::*;
use jsonrpc_http_server::*;

use serde_json::json;
use snafu::ResultExt;

use derivative::*;
use solana_evm_loader_program::scope::*;
use solana_sdk::{
    clock::MS_PER_TICK, fee_calculator::DEFAULT_TARGET_LAMPORTS_PER_SIGNATURE, pubkey::Pubkey,
    signers::Signers, transaction::TransactionError,
};

use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    rpc_client::RpcClient,
    rpc_config::*,
    rpc_request::{RpcRequest, RpcResponseErrorData},
    rpc_response::Response as RpcResponse,
    rpc_response::*,
};

use tracing_attributes::instrument;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{
    filter::{LevelFilter, Targets},
    layer::{Layer, SubscriberExt},
};

use ::tokio;
use ::tokio::sync::mpsc;

use pool::{
    worker_cleaner, worker_deploy, worker_signature_checker, EthPool, PooledTransaction,
    SystemClock,
};

use rlp::Encodable;
use secp256k1::Message;
use std::result::Result as StdResult;
type EvmResult<T> = StdResult<T, evm_rpc::Error>;

const MAX_NUM_BLOCKS_IN_BATCH: u64 = 2000; // should be less or equal to const core::evm_rpc_impl::logs::MAX_NUM_BLOCKS

// A compatibility layer, to make software more fluently.
mod compatibility {
    use evm_rpc::Hex;
    use evm_state::{Gas, TransactionAction, H256, U256};
    use rlp::{Decodable, DecoderError, Rlp};

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
    pub struct TransactionSignature {
        pub v: u64,
        pub r: U256,
        pub s: U256,
    }
    #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
    pub struct Transaction {
        pub nonce: U256,
        pub gas_price: Gas,
        pub gas_limit: Gas,
        pub action: TransactionAction,
        pub value: U256,
        pub signature: TransactionSignature,
        pub input: Vec<u8>,
    }

    impl Decodable for Transaction {
        fn decode(rlp: &Rlp) -> Result<Self, DecoderError> {
            Ok(Self {
                nonce: rlp.val_at(0)?,
                gas_price: rlp.val_at(1)?,
                gas_limit: rlp.val_at(2)?,
                action: rlp.val_at(3)?,
                value: rlp.val_at(4)?,
                input: rlp.val_at(5)?,
                signature: TransactionSignature {
                    v: rlp.val_at(6)?,
                    r: rlp.val_at(7)?,
                    s: rlp.val_at(8)?,
                },
            })
        }
    }

    impl From<Transaction> for evm_state::Transaction {
        fn from(tx: Transaction) -> evm_state::Transaction {
            let mut r = [0u8; 32];
            let mut s = [0u8; 32];
            tx.signature.r.to_big_endian(&mut r);
            tx.signature.s.to_big_endian(&mut s);
            evm_state::Transaction {
                nonce: tx.nonce,
                gas_limit: tx.gas_limit,
                gas_price: tx.gas_price,
                action: tx.action,
                value: tx.value,
                input: tx.input,
                signature: evm_state::TransactionSignature {
                    v: tx.signature.v,
                    r: r.into(),
                    s: s.into(),
                },
            }
        }
    }

    pub fn patch_tx(mut tx: evm_rpc::RPCTransaction) -> evm_rpc::RPCTransaction {
        if tx.r.unwrap_or_default() == Hex(U256::zero()) {
            tx.r = Some(Hex(0x1.into()))
        }
        if tx.s.unwrap_or_default() == Hex(U256::zero()) {
            tx.s = Some(Hex(0x1.into()))
        }
        tx
    }

    pub fn patch_block(mut block: evm_rpc::RPCBlock) -> evm_rpc::RPCBlock {
        let txs_empty = match &block.transactions {
            evm_rpc::Either::Left(txs) => txs.is_empty(),
            evm_rpc::Either::Right(txs) => txs.is_empty(),
        };
        // if no tx, and its root == zero, return empty trie hash, to avoid panics in go client.
        if txs_empty && block.transactions_root.0 == H256::zero() {
            evm_rpc::RPCBlock {
                transactions_root: Hex(evm_state::empty_trie_hash()),
                receipts_root: Hex(evm_state::empty_trie_hash()),
                ..block
            }
        } else {
            // if txs exist, check that their signatures are not zero, and fix them if so.
            block.transactions = match block.transactions {
                evm_rpc::Either::Left(txs) => evm_rpc::Either::Left(txs),
                evm_rpc::Either::Right(txs) => {
                    evm_rpc::Either::Right(txs.into_iter().map(patch_tx).collect())
                }
            };
            block
        }
    }
}

macro_rules! proxy_evm_rpc {
    (@silent $rpc: expr, $rpc_call:ident $(, $calls:expr)*) => (
        {
            match RpcClient::send(&$rpc, RpcRequest::$rpc_call, json!([$($calls,)*])) {
                Err(e) => Err(from_client_error(e).into()),
                Ok(o) => Ok(o)
            }
        }
    );
    ($rpc: expr, $rpc_call:ident $(, $calls:expr)*) => (
        {
            debug!("evm proxy received {}", stringify!($rpc_call));
            proxy_evm_rpc!(@silent $rpc, $rpc_call $(, $calls)* )
        }
    )

}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct EvmBridge {
    evm_chain_id: u64,
    key: solana_sdk::signature::Keypair,
    accounts: HashMap<evm_state::Address, evm_state::SecretKey>,

    #[derivative(Debug = "ignore")]
    rpc_client: RpcClient,
    verbose_errors: bool,
    simulate: bool,
    max_logs_blocks: u64,
    pool: EthPool<SystemClock>,
    min_gas_price: U256,
}

impl EvmBridge {
    fn new(
        evm_chain_id: u64,
        keypath: &str,
        evm_keys: Vec<SecretKey>,
        addr: String,
        verbose_errors: bool,
        simulate: bool,
        max_logs_blocks: u64,
        min_gas_price: U256,
    ) -> Self {
        info!("EVM chain id {}", evm_chain_id);

        let accounts = evm_keys
            .into_iter()
            .map(|secret_key| {
                let public_key =
                    evm_state::PublicKey::from_secret_key(evm_state::SECP256K1, &secret_key);
                let public_key = evm_state::addr_from_public_key(&public_key);
                (public_key, secret_key)
            })
            .collect();

        info!("Trying to create rpc client with addr: {}", addr);
        let rpc_client = RpcClient::new_with_commitment(addr, CommitmentConfig::processed());

        info!("Loading keypair from: {}", keypath);
        let key = solana_sdk::signature::read_keypair_file(&keypath).unwrap();

        info!("Creating mempool...");
        let pool = EthPool::new(SystemClock);

        Self {
            evm_chain_id,
            key,
            accounts,
            rpc_client,
            verbose_errors,
            simulate,
            max_logs_blocks,
            pool,
            min_gas_price,
        }
    }

    /// Wrap evm tx into solana, optionally add meta keys, to solana signature.
    async fn send_tx(
        &self,
        tx: evm::Transaction,
        meta_keys: HashSet<Pubkey>,
    ) -> EvmResult<Hex<H256>> {
        let (sender, mut receiver) = mpsc::channel::<EvmResult<Hex<H256>>>(1);

        if tx.gas_price < self.min_gas_price {
            return Err(Error::GasPriceTooLow {
                need: self.min_gas_price,
            });
        }

        let tx = PooledTransaction::new(tx, meta_keys, sender)
            .map_err(|source| evm_rpc::Error::EvmStateError { source })?;
        let tx = match self.pool.import(tx) {
            // tx was already processed on this bridge, return hash.
            Err(txpool::Error::AlreadyImported(h)) => return Ok(Hex(h)),
            Ok(tx) => tx,
            Err(source) => {
                warn!("Could not import tx to the pool");
                return Err(evm_rpc::Error::RuntimeError {
                    details: format!("Mempool error: {:?}", source),
                });
            }
        };

        if self.simulate {
            receiver.recv().await.unwrap()
        } else {
            Ok(tx.inner.tx_id_hash().into())
        }
    }

    fn block_to_number(&self, block: Option<BlockId>) -> EvmResult<u64> {
        let block = block.unwrap_or_default();
        let block_num = match block {
            BlockId::Num(block) => block.0,
            BlockId::RelativeId(BlockRelId::Latest) => {
                let num: Hex<u64> = proxy_evm_rpc!(self.rpc_client, EthBlockNumber)?;
                num.0
            }
            _ => return Err(Error::BlockNotFound { block }),
        };
        Ok(block_num)
    }

    pub fn is_transaction_landed(&self, hash: &H256) -> Option<bool> {
        fn is_receipt_exists(bridge: &EvmBridge, hash: &H256) -> Option<bool> {
            bridge
                .rpc_client
                .get_evm_transaction_receipt(hash)
                .ok()
                .flatten()
                .map(|_receipt| true)
        }

        fn is_signature_exists(bridge: &EvmBridge, hash: &H256) -> Option<bool> {
            bridge
                .pool
                .signature_of_cached_transaction(hash)
                .map(|signature| {
                    bridge
                        .rpc_client
                        .get_signature_status(&signature)
                        .ok()
                        .flatten()
                        .map(|result| result.ok())
                        .flatten()
                        .map(|()| true)
                })
                .flatten()
        }

        is_receipt_exists(self, hash).or_else(|| is_signature_exists(self, hash))
    }
}

#[derive(Debug)]
pub struct BridgeErpcImpl;

impl BridgeERPC for BridgeErpcImpl {
    type Metadata = Arc<EvmBridge>;

    #[instrument]
    fn accounts(&self, meta: Self::Metadata) -> EvmResult<Vec<Hex<Address>>> {
        Ok(meta.accounts.iter().map(|(k, _)| Hex(*k)).collect())
    }

    #[instrument]
    fn sign(&self, meta: Self::Metadata, address: Hex<Address>, data: Bytes) -> EvmResult<Bytes> {
        let secret_key = meta
            .accounts
            .get(&address.0)
            .ok_or(Error::KeyNotFound { account: address.0 })?;
        let mut message_data =
            format!("\x19Ethereum Signed Message:\n{}", data.0.len()).into_bytes();
        message_data.extend_from_slice(&data.0);
        let hash_to_sign = solana_sdk::keccak::hash(&message_data);
        let msg: Message = Message::from_slice(&hash_to_sign.to_bytes()).unwrap();
        let sig = SECP256K1.sign_recoverable(&msg, &secret_key);
        let (rid, sig) = { sig.serialize_compact() };

        let mut sig_data_arr = [0; 65];
        sig_data_arr[0..64].copy_from_slice(&sig[0..64]);
        sig_data_arr[64] = rid.to_i32() as u8;
        Ok(sig_data_arr.to_vec().into())
    }

    #[instrument]
    fn sign_transaction(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
    ) -> BoxFuture<EvmResult<Bytes>> {
        let future = async move {
            let address = tx.from.map(|a| a.0).unwrap_or_default();

            debug!("sign_transaction from = {}", address);

            let secret_key = meta
                .accounts
                .get(&address)
                .ok_or(Error::KeyNotFound { account: address })?;

            let nonce = tx
                .nonce
                .map(|a| a.0)
                .or_else(|| meta.pool.transaction_count(&address))
                .or_else(|| meta.rpc_client.get_evm_transaction_count(&address).ok())
                .unwrap_or_default();

            let tx = UnsignedTransaction {
                nonce,
                gas_price: tx
                    .gas_price
                    .map(|a| a.0)
                    .unwrap_or_else(|| meta.min_gas_price),
                gas_limit: tx.gas.map(|a| a.0).unwrap_or_else(|| 30000000.into()),
                action: tx
                    .to
                    .map(|a| TransactionAction::Call(a.0))
                    .unwrap_or(TransactionAction::Create),
                value: tx.value.map(|a| a.0).unwrap_or_else(|| 0.into()),
                input: tx.input.map(|a| a.0).unwrap_or_default(),
            };

            let tx = tx.sign(secret_key, Some(meta.evm_chain_id));
            Ok(tx.rlp_bytes().to_vec().into())
        };
        Box::pin(future)
    }

    #[instrument]
    fn send_transaction(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
        meta_keys: Option<Vec<String>>,
    ) -> BoxFuture<EvmResult<Hex<H256>>> {
        let future = async move {
            let address = tx.from.map(|a| a.0).unwrap_or_default();

            debug!("send_transaction from = {}", address);

            let meta_keys = meta_keys
                .into_iter()
                .flatten()
                .map(|s| solana_sdk::pubkey::Pubkey::from_str(&s))
                .collect::<StdResult<HashSet<_>, _>>()
                .map_err(|e| into_native_error(e, meta.verbose_errors))?;

            let secret_key = meta
                .accounts
                .get(&address)
                .ok_or(Error::KeyNotFound { account: address })?;

            let nonce = tx
                .nonce
                .map(|a| a.0)
                .or_else(|| meta.pool.transaction_count(&address))
                .or_else(|| meta.rpc_client.get_evm_transaction_count(&address).ok())
                .unwrap_or_default();

            let tx_create = evm::UnsignedTransaction {
                nonce,
                gas_price: tx
                    .gas_price
                    .map(|a| a.0)
                    .unwrap_or_else(|| meta.min_gas_price),
                gas_limit: tx.gas.map(|a| a.0).unwrap_or_else(|| 30000000.into()),
                action: tx
                    .to
                    .map(|a| evm::TransactionAction::Call(a.0))
                    .unwrap_or(evm::TransactionAction::Create),
                value: tx.value.map(|a| a.0).unwrap_or_else(|| 0.into()),
                input: tx.input.map(|a| a.0).unwrap_or_default(),
            };

            let tx = tx_create.sign(secret_key, Some(meta.evm_chain_id));

            meta.send_tx(tx, meta_keys).await
        };

        Box::pin(future)
    }

    #[instrument]
    fn send_raw_transaction(
        &self,
        meta: Self::Metadata,
        bytes: Bytes,
        meta_keys: Option<Vec<String>>,
    ) -> BoxFuture<EvmResult<Hex<H256>>> {
        let future = async move {
            debug!("send_raw_transaction");
            let meta_keys = meta_keys
                .into_iter()
                .flatten()
                .map(|s| solana_sdk::pubkey::Pubkey::from_str(&s))
                .collect::<StdResult<HashSet<_>, _>>()
                .map_err(|e| into_native_error(e, meta.verbose_errors))?;

            let tx: compatibility::Transaction =
                rlp::decode(&bytes.0).with_context(|| RlpError {
                    struct_name: "RawTransaction".to_string(),
                    input_data: hex::encode(&bytes.0),
                })?;
            let tx: evm::Transaction = tx.into();

            // TODO: Check chain_id.
            // TODO: check gas price.

            let unsigned_tx: evm::UnsignedTransaction = tx.clone().into();
            let hash = unsigned_tx.signing_hash(Some(meta.evm_chain_id));
            debug!("loaded tx_hash = {}", hash);

            meta.send_tx(tx, meta_keys).await
        };

        Box::pin(future)
    }

    #[instrument]
    fn compilers(&self, _meta: Self::Metadata) -> EvmResult<Vec<String>> {
        Ok(vec![])
    }
}

#[derive(Debug)]
pub struct GeneralErpcProxy;
impl GeneralERPC for GeneralErpcProxy {
    type Metadata = Arc<EvmBridge>;

    #[instrument]
    fn network_id(&self, meta: Self::Metadata) -> EvmResult<String> {
        // NOTE: also we can get chain id from meta, but expects the same value
        Ok(format!("{}", meta.evm_chain_id))
    }

    #[instrument]
    // TODO: Add network info
    fn is_listening(&self, _meta: Self::Metadata) -> EvmResult<bool> {
        Ok(true)
    }

    #[instrument]
    fn peer_count(&self, _meta: Self::Metadata) -> EvmResult<Hex<usize>> {
        Ok(Hex(0))
    }

    #[instrument]
    fn chain_id(&self, meta: Self::Metadata) -> EvmResult<Hex<u64>> {
        Ok(Hex(meta.evm_chain_id))
    }

    #[instrument]
    fn sha3(&self, _meta: Self::Metadata, bytes: Bytes) -> EvmResult<Hex<H256>> {
        Ok(Hex(H256::from_slice(
            Keccak256::digest(bytes.0.as_slice()).as_slice(),
        )))
    }

    #[instrument]
    fn client_version(&self, _meta: Self::Metadata) -> EvmResult<String> {
        Ok(String::from("VelasEvm/v0.5.0"))
    }

    #[instrument]
    fn protocol_version(&self, _meta: Self::Metadata) -> EvmResult<String> {
        Ok(solana_version::semver!().into())
    }

    #[instrument]
    fn is_syncing(&self, meta: Self::Metadata) -> EvmResult<bool> {
        proxy_evm_rpc!(meta.rpc_client, EthSyncing)
    }

    #[instrument]
    fn coinbase(&self, _meta: Self::Metadata) -> EvmResult<Hex<Address>> {
        Ok(Hex(Address::from_low_u64_be(0)))
    }

    #[instrument]
    fn is_mining(&self, _meta: Self::Metadata) -> EvmResult<bool> {
        Ok(false)
    }

    #[instrument]
    fn hashrate(&self, _meta: Self::Metadata) -> EvmResult<Hex<U256>> {
        Ok(Hex(U256::zero()))
    }

    #[instrument]
    fn gas_price(&self, meta: Self::Metadata) -> EvmResult<Hex<Gas>> {
        Ok(Hex(meta.min_gas_price))
    }
}

#[derive(Debug)]
pub struct ChainErpcProxy;
impl ChainERPC for ChainErpcProxy {
    type Metadata = Arc<EvmBridge>;

    #[instrument]
    // The same as get_slot
    fn block_number(&self, meta: Self::Metadata) -> BoxFuture<EvmResult<Hex<usize>>> {
        Box::pin(ready(proxy_evm_rpc!(meta.rpc_client, EthBlockNumber)))
    }

    #[instrument]
    fn balance(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        block: Option<BlockId>,
    ) -> BoxFuture<EvmResult<Hex<U256>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetBalance,
            address,
            block
        )))
    }

    #[instrument]
    fn storage_at(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        data: Hex<U256>,
        block: Option<BlockId>,
    ) -> BoxFuture<EvmResult<Hex<H256>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetStorageAt,
            address,
            data,
            block
        )))
    }

    #[instrument]
    fn transaction_count(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        block: Option<BlockId>,
    ) -> BoxFuture<EvmResult<Hex<U256>>> {
        if matches!(block, Some(BlockId::RelativeId(BlockRelId::Pending))) {
            if let Some(tx_count) = meta.pool.transaction_count(&address.0) {
                return Box::pin(ready(Ok(Hex(tx_count))));
            }
        }

        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetTransactionCount,
            address,
            block
        )))
    }

    #[instrument]
    fn block_transaction_count_by_number(
        &self,
        meta: Self::Metadata,
        block: BlockId,
    ) -> BoxFuture<EvmResult<Hex<usize>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetBlockTransactionCountByNumber,
            block
        )))
    }

    #[instrument]
    fn block_transaction_count_by_hash(
        &self,
        meta: Self::Metadata,
        block_hash: Hex<H256>,
    ) -> BoxFuture<EvmResult<Hex<usize>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetBlockTransactionCountByHash,
            block_hash
        )))
    }

    #[instrument]
    fn code(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        block: Option<BlockId>,
    ) -> BoxFuture<EvmResult<Bytes>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetCode,
            address,
            block
        )))
    }

    #[instrument]
    fn block_by_hash(
        &self,
        meta: Self::Metadata,
        block_hash: Hex<H256>,
        full: bool,
    ) -> BoxFuture<EvmResult<Option<RPCBlock>>> {
        if block_hash == Hex(H256::zero()) {
            Box::pin(ready(Ok(Some(RPCBlock::default()))))
        } else {
            Box::pin(ready(
                proxy_evm_rpc!(meta.rpc_client, EthGetBlockByHash, block_hash, full)
                    .map(|o: Option<_>| o.map(compatibility::patch_block)),
            ))
        }
    }

    #[instrument]
    fn block_by_number(
        &self,
        meta: Self::Metadata,
        block: BlockId,
        full: bool,
    ) -> BoxFuture<EvmResult<Option<RPCBlock>>> {
        if block == BlockId::Num(0x0.into()) {
            Box::pin(ready(Ok(Some(RPCBlock::default()))))
        } else {
            Box::pin(ready(
                proxy_evm_rpc!(meta.rpc_client, EthGetBlockByNumber, block, full)
                    .map(|o: Option<_>| o.map(compatibility::patch_block)),
            ))
        }
    }

    #[instrument]
    fn transaction_by_hash(
        &self,
        meta: Self::Metadata,
        tx_hash: Hex<H256>,
    ) -> BoxFuture<EvmResult<Option<RPCTransaction>>> {
        // TODO: chain all possible outcomes properly
        if let Some(tx) = meta.pool.transaction_by_hash(tx_hash) {
            if let Ok(tx) = RPCTransaction::from_transaction((**tx).clone().into()) {
                // TODO: should we `patch` tx?
                return Box::pin(ready(Ok(Some(tx))));
            }
        }
        Box::pin(ready(
            proxy_evm_rpc!(meta.rpc_client, EthGetTransactionByHash, tx_hash)
                .map(|o: Option<_>| o.map(compatibility::patch_tx)),
        ))
    }

    #[instrument]
    fn transaction_by_block_hash_and_index(
        &self,
        meta: Self::Metadata,
        block_hash: Hex<H256>,
        tx_id: Hex<usize>,
    ) -> BoxFuture<EvmResult<Option<RPCTransaction>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetTransactionByBlockHashAndIndex,
            block_hash,
            tx_id
        )))
    }

    #[instrument]
    fn transaction_by_block_number_and_index(
        &self,
        meta: Self::Metadata,
        block: BlockId,
        tx_id: Hex<usize>,
    ) -> BoxFuture<EvmResult<Option<RPCTransaction>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetTransactionByBlockNumberAndIndex,
            block,
            tx_id
        )))
    }

    #[instrument]
    fn transaction_receipt(
        &self,
        meta: Self::Metadata,
        tx_hash: Hex<H256>,
    ) -> BoxFuture<EvmResult<Option<RPCReceipt>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthGetTransactionReceipt,
            tx_hash
        )))
    }

    #[instrument]
    fn call(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
        block: Option<BlockId>,
        meta_keys: Option<Vec<String>>,
    ) -> BoxFuture<EvmResult<Bytes>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthCall,
            tx,
            block,
            meta_keys
        )))
    }

    #[instrument]
    fn estimate_gas(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
        block: Option<BlockId>,
        meta_keys: Option<Vec<String>>,
    ) -> BoxFuture<EvmResult<Hex<Gas>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthEstimateGas,
            tx,
            block,
            meta_keys
        )))
    }

    #[instrument(skip(self, meta))]
    fn logs(
        &self,
        meta: Self::Metadata,
        mut log_filter: RPCLogFilter,
    ) -> BoxFuture<EvmResult<Vec<RPCLog>>> {
        let starting_block = match meta.block_to_number(log_filter.from_block) {
            Ok(res) => res,
            Err(err) => return Box::pin(ready(Err(err))),
        };
        let ending_block = match meta.block_to_number(log_filter.to_block) {
            Ok(res) => res,
            Err(err) => return Box::pin(ready(Err(err))),
        };

        if ending_block < starting_block {
            return Box::pin(ready(Err(Error::InvalidBlocksRange {
                starting: starting_block,
                ending: ending_block,
                batch_size: None,
            })));
        }

        // request more than we can provide
        if ending_block > starting_block + meta.max_logs_blocks {
            return Box::pin(ready(Err(Error::InvalidBlocksRange {
                starting: starting_block,
                ending: ending_block,
                batch_size: Some(meta.max_logs_blocks),
            })));
        }

        let mut starting = starting_block;

        // make execution parallel
        Box::pin(async move {
            let mut collector = Vec::new();
            while starting <= ending_block {
                let ending = (starting.saturating_add(MAX_NUM_BLOCKS_IN_BATCH)).min(ending_block);
                log_filter.from_block = Some(starting.into());
                log_filter.to_block = Some(ending.into());

                let cloned_filter = log_filter.clone();
                let cloned_meta = meta.clone();
                // Parallel execution:
                collector.push(tokio::task::spawn_blocking(move || {
                    info!("filter = {:?}", cloned_filter);
                    let result: EvmResult<Vec<RPCLog>> =
                        proxy_evm_rpc!(@silent cloned_meta.rpc_client, EthGetLogs, cloned_filter);
                    info!("logs = {:?}", result);

                    result
                }));

                starting = starting.saturating_add(MAX_NUM_BLOCKS_IN_BATCH + 1);
            }
            // join all execution, fast fail on any error.
            let mut result = Vec::new();
            for collection in collector {
                result.extend(collection.await.map_err(|details| Error::RuntimeError {
                    details: details.to_string(),
                })??)
            }
            Ok(result)
        })
    }

    #[instrument]
    fn uncle_by_block_hash_and_index(
        &self,
        _meta: Self::Metadata,
        _block_hash: Hex<H256>,
        _uncle_id: Hex<U256>,
    ) -> EvmResult<Option<RPCBlock>> {
        Ok(None)
    }

    #[instrument]
    fn uncle_by_block_number_and_index(
        &self,
        _meta: Self::Metadata,
        _block: String,
        _uncle_id: Hex<U256>,
    ) -> EvmResult<Option<RPCBlock>> {
        Ok(None)
    }

    #[instrument]
    fn block_uncles_count_by_hash(
        &self,
        _meta: Self::Metadata,
        _block_hash: Hex<H256>,
    ) -> EvmResult<Hex<usize>> {
        Ok(Hex(0))
    }

    #[instrument]
    fn block_uncles_count_by_number(
        &self,
        _meta: Self::Metadata,
        _block: String,
    ) -> EvmResult<Hex<usize>> {
        Ok(Hex(0))
    }
}

#[derive(Debug)]
pub struct TraceErpcProxy;
impl TraceERPC for TraceErpcProxy {
    type Metadata = Arc<EvmBridge>;

    #[instrument]
    fn trace_call(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
        traces: Vec<String>,
        block: Option<BlockId>,
        meta_info: Option<TraceMeta>,
    ) -> BoxFuture<EvmResult<evm_rpc::trace::TraceResultsWithTransactionHash>> {
        Box::pin(ready(proxy_evm_rpc!(meta.rpc_client, EthTraceCall, tx, traces, block, meta_info)))
    }

    #[instrument]
    fn trace_call_many(
        &self,
        meta: Self::Metadata,
        tx_traces: Vec<(RPCTransaction, Vec<String>, Option<TraceMeta>)>,
        block: Option<BlockId>,
    ) -> BoxFuture<EvmResult<Vec<evm_rpc::trace::TraceResultsWithTransactionHash>>> {
        Box::pin(ready(proxy_evm_rpc!(meta.rpc_client, EthTraceCallMany, tx_traces, block)))
    }

    #[instrument]
    fn trace_replay_transaction(
        &self,
        meta: Self::Metadata,
        tx_hash: Hex<H256>,
        traces: Vec<String>,
        meta_info: Option<TraceMeta>,
    ) -> BoxFuture<EvmResult<Option<trace::TraceResultsWithTransactionHash>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthTraceReplayTransaction,
            tx_hash,
            traces,
            meta_info
        )))
    }

    #[instrument]
    fn trace_replay_block(
        &self,
        meta: Self::Metadata,
        block: BlockId,
        traces: Vec<String>,
        meta_info: Option<TraceMeta>,
    ) -> BoxFuture<EvmResult<Vec<trace::TraceResultsWithTransactionHash>>> {
        Box::pin(ready(proxy_evm_rpc!(
            meta.rpc_client,
            EthTraceReplayBlock,
            block,
            traces,
            meta_info
        )))
    }
}

pub(crate) fn from_client_error(client_error: ClientError) -> evm_rpc::Error {
    let client_error_kind = client_error.kind();
    match client_error_kind {
        ClientErrorKind::RpcError(solana_client::rpc_request::RpcError::RpcResponseError {
            code,
            message,
            data,
            original_err,
        }) => {
            match data {
                // if transaction preflight, try to get last log messages, and return it as error.
                RpcResponseErrorData::SendTransactionPreflightFailure(
                    RpcSimulateTransactionResult {
                        err: Some(TransactionError::InstructionError(_, _)),
                        logs: Some(logs),
                        ..
                    },
                ) if !logs.is_empty() => {
                    let first_entry = logs.len().saturating_sub(2); // take two elements from logs
                    let last_log = logs[first_entry..].join(";");

                    return evm_rpc::Error::ProxyRpcError {
                        source: jsonrpc_core::Error {
                            code: (*code).into(),
                            message: last_log,
                            data: original_err.clone().into(),
                        },
                    };
                }
                _ => {}
            }
            evm_rpc::Error::ProxyRpcError {
                source: jsonrpc_core::Error {
                    code: (*code).into(),
                    message: message.clone(),
                    data: original_err.clone().into(),
                },
            }
        }
        _ => evm_rpc::Error::NativeRpcError {
            details: format!("{:?}", client_error),
            source: client_error.into(),
            verbose: false, // don't verbose native errors.
        },
    }
}

#[derive(Debug, structopt::StructOpt)]
struct Args {
    keyfile: Option<String>,
    #[structopt(default_value = "http://127.0.0.1:8899")]
    rpc_address: String,
    #[structopt(default_value = "127.0.0.1:8545")]
    binding_address: SocketAddr,
    #[structopt(default_value = "57005")] // 0xdead
    evm_chain_id: u64,
    #[structopt(long = "min-gas-price")]
    min_gas_price: Option<String>,
    #[structopt(long = "verbose-errors")]
    verbose_errors: bool,
    #[structopt(long = "no-simulate")]
    no_simulate: bool, // parse inverted to keep false default
    /// Maximum number of blocks to return in eth_getLogs rpc.
    #[structopt(long = "max-logs-block-count", default_value = "500")]
    max_logs_blocks: u64,

    #[structopt(long = "jaeger-collector-url", short = "j")]
    jaeger_collector_url: Option<String>,
}

impl Args {
    fn min_gas_price_or_default(&self) -> U256 {
        let gwei: U256 = 1_000_000_000.into();
        fn min_gas_price() -> U256 {
            //TODO: Add gas logic
            (21000 * solana_evm_loader_program::scope::evm::LAMPORTS_TO_GWEI_PRICE
                / DEFAULT_TARGET_LAMPORTS_PER_SIGNATURE)
                .into() // 21000 is smallest call in evm
        }

        let mut gas_price = match self
            .min_gas_price
            .as_ref()
            .and_then(|gas_price| U256::from_dec_str(gas_price).ok())
        {
            Some(gas_price) => {
                info!(r#"--min-gas-price is set to {}"#, &gas_price);
                gas_price
            }
            None => {
                let default_price = min_gas_price();
                warn!(
                    r#"Value of "--min-gas-price" is not set or unable to parse. Default value is: {}"#,
                    default_price
                );
                default_price
            }
        };
        // ceil to gwei for metamask
        gas_price += gwei - 1;
        gas_price - gas_price % gwei
    }
}

const SECRET_KEY_DUMMY: [u8; 32] = [1; 32];

#[paw::main]
#[tokio::main]
async fn main(args: Args) -> StdResult<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let min_gas_price = args.min_gas_price_or_default();
    let keyfile_path = args
        .keyfile
        .unwrap_or_else(|| solana_cli_config::Config::default().keypair_path);
    let server_path = args.rpc_address;
    let binding_address = args.binding_address;

    if let Some(collector) = args.jaeger_collector_url {
        // init tracer
        let fmt_filter = std::env::var("RUST_LOG")
            .ok()
            .and_then(|rust_log| match rust_log.parse::<Targets>() {
                Ok(targets) => Some(targets),
                Err(e) => {
                    eprintln!("failed to parse `RUST_LOG={:?}`: {}", rust_log, e);
                    None
                }
            })
            .unwrap_or_else(|| Targets::default().with_default(LevelFilter::WARN));

        let tracer = opentelemetry_jaeger::new_pipeline()
            .with_service_name("evm-bridge-tracer")
            .with_collector_endpoint(collector)
            .install_batch(opentelemetry::runtime::Tokio)
            .unwrap();
        let opentelemetry = tracing_opentelemetry::layer().with_tracer(tracer);
        let registry = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(fmt_filter))
            .with(opentelemetry);

        registry.try_init().unwrap();
    }

    let meta = EvmBridge::new(
        args.evm_chain_id,
        &keyfile_path,
        vec![evm::SecretKey::from_slice(&SECRET_KEY_DUMMY).unwrap()],
        server_path,
        args.verbose_errors,
        !args.no_simulate, // invert argument
        args.max_logs_blocks,
        min_gas_price,
    );
    let meta = Arc::new(meta);

    let mut io = MetaIoHandler::default();

    {
        use solana_core::rpc::rpc_minimal::Minimal;
        let minimal_rpc = sol_proxy::MinimalRpcSolProxy;
        io.extend_with(minimal_rpc.to_delegate());
    }
    {
        use solana_core::rpc::rpc_full::Full;
        let full_rpc = sol_proxy::FullRpcSolProxy;
        io.extend_with(full_rpc.to_delegate());
    }

    let ether_bridge = BridgeErpcImpl;
    io.extend_with(ether_bridge.to_delegate());
    let ether_chain = ChainErpcProxy;
    io.extend_with(ether_chain.to_delegate());
    let ether_general = GeneralErpcProxy;
    io.extend_with(ether_general.to_delegate());
    let ether_trace = TraceErpcProxy;
    io.extend_with(ether_trace.to_delegate());

    let mempool_worker = worker_deploy(meta.clone());

    let cleaner = worker_cleaner(meta.clone());

    let signature_checker = worker_signature_checker(meta.clone());

    info!("Creating server with: {}", binding_address);
    let meta_clone = meta.clone();
    let server = ServerBuilder::with_meta_extractor(
        io.clone(),
        move |_req: &hyper::Request<hyper::Body>| meta_clone.clone(),
    )
    .cors(DomainsValidation::AllowOnly(vec![
        AccessControlAllowOrigin::Any,
    ]))
    .threads(4)
    .cors_max_age(86400)
    .start_http(&binding_address)
    .expect("Unable to start EVM bridge server");

    let ws_server = {
        let mut websocket_binding = binding_address;
        websocket_binding.set_port(binding_address.port() + 1);
        info!("Creating websocket server: {}", websocket_binding);
        jsonrpc_ws_server::ServerBuilder::with_meta_extractor(io, move |_: &_| meta.clone())
            .start(&websocket_binding)
            .expect("Unable to start EVM bridge server")
    };

    let _cleaner = tokio::task::spawn(cleaner);
    let _signature_checker = tokio::task::spawn(signature_checker);
    let mempool_task = tokio::task::spawn(mempool_worker);
    let servers_waiter = tokio::task::spawn_blocking(|| {
        ws_server.wait().unwrap();
        server.wait();
    });

    // wait for any failure/stops.
    tokio::select! {
        _ = servers_waiter => {
            println!("Server exited.");
        }
        _ = mempool_task => {
            println!("Mempool task exited.");
        }
    };
    Ok(())
}

fn send_and_confirm_transactions<T: Signers>(
    rpc_client: &RpcClient,
    mut transactions: Vec<solana::Transaction>,
    signer_keys: &T,
) -> StdResult<(), anyhow::Error> {
    const SEND_RETRIES: usize = 5;
    const STATUS_RETRIES: usize = 15;

    for _ in 0..SEND_RETRIES {
        // Send all transactions
        let mut transactions_signatures = transactions
            .drain(..)
            .map(|transaction| {
                if cfg!(not(test)) {
                    // Delay ~1 tick between write transactions in an attempt to reduce AccountInUse errors
                    // when all the write transactions modify the same program account (eg, deploying a
                    // new program)
                    sleep(Duration::from_millis(MS_PER_TICK));
                }

                debug!("Sending {:?}", transaction.signatures);

                let signature = rpc_client
                    .send_transaction_with_config(
                        &transaction,
                        RpcSendTransactionConfig {
                            skip_preflight: true, // NOTE: was true
                            ..RpcSendTransactionConfig::default()
                        },
                    )
                    .map_err(|e| error!("Send transaction error: {:?}", e))
                    .ok();

                (transaction, signature)
            })
            .collect::<Vec<_>>();

        for _ in 0..STATUS_RETRIES {
            // Collect statuses for all the transactions, drop those that are confirmed

            if cfg!(not(test)) {
                // Retry twice a second
                sleep(Duration::from_millis(500));
            }

            transactions_signatures.retain(|(_transaction, signature)| {
                signature
                    .and_then(|signature| rpc_client.get_signature_statuses(&[signature]).ok())
                    .and_then(|RpcResponse { mut value, .. }| value.remove(0))
                    .and_then(|status| status.confirmations)
                    .map(|confirmations| confirmations == 0) // retain unconfirmed only
                    .unwrap_or(true)
            });

            if transactions_signatures.is_empty() {
                return Ok(());
            }
        }

        // Re-sign any failed transactions with a new blockhash and retry
        let (blockhash, _) = rpc_client
            .get_new_blockhash(&transactions_signatures[0].0.message().recent_blockhash)?;

        for (mut transaction, _) in transactions_signatures {
            transaction.try_sign(signer_keys, blockhash)?;
            debug!("Resending {:?}", transaction);
            transactions.push(transaction);
        }
    }
    Err(anyhow::Error::msg("Transactions failed"))
}

#[cfg(test)]
mod tests {
    use crate::{BridgeErpcImpl, EthPool, EvmBridge, SystemClock};
    use evm_rpc::{BridgeERPC, Hex};
    use evm_state::Address;
    use secp256k1::SecretKey;
    use solana_client::rpc_client::RpcClient;
    use solana_sdk::signature::Keypair;
    use std::str::FromStr;
    use std::sync::Arc;

    #[test]
    fn test_eth_sign() {
        let signing_key =
            SecretKey::from_str("c21020a52198632ae7d5c1adaa3f83da2e0c98cf541c54686ddc8d202124c086")
                .unwrap();
        let public_key = evm_state::PublicKey::from_secret_key(evm_state::SECP256K1, &signing_key);
        let public_key = evm_state::addr_from_public_key(&public_key);
        let bridge = Arc::new(EvmBridge {
            evm_chain_id: 111u64,
            key: Keypair::new(),
            accounts: vec![(public_key, signing_key)].into_iter().collect(),
            rpc_client: RpcClient::new("".to_string()),
            verbose_errors: true,
            simulate: false,
            max_logs_blocks: 0u64,
            pool: EthPool::new(SystemClock),
            min_gas_price: 0.into(),
        });

        let rpc = BridgeErpcImpl {};
        let address = Address::from_str("0x141a4802f84bb64c0320917672ef7D92658e964e").unwrap();
        let data = "qwe".as_bytes().to_vec();
        let res = rpc.sign(bridge, Hex(address), data.into()).unwrap();
        assert_eq!(res.to_string(), "0xb734e224f0f92d89825f3f69bf03924d7d2f609159d6ce856d37a58d7fcbc8eb6d224fd73f05217025ed015283133c92888211b238272d87ec48347f05ab42a000");
    }
}
