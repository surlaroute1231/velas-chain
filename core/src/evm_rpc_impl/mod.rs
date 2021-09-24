use std::str::FromStr;

use sha3::{Digest, Keccak256};
use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_sdk::keyed_account::KeyedAccount;

use crate::rpc::JsonRpcRequestProcessor;
use evm_rpc::error::EvmStateError;
use evm_rpc::{
    basic::BasicERPC,
    chain_mock::ChainMockERPC,
    error::{Error, IntoNativeRpcError},
    trace::TraceMeta,
    BlockId, BlockRelId, Bytes, Either, Hex, RPCBlock, RPCLog, RPCLogFilter, RPCReceipt,
    RPCTopicFilter, RPCTransaction,
};
use evm_state::{AccountProvider, Address, Gas, LogFilter, TransactionAction, H256, U256};
use snafu::ResultExt;
use solana_runtime::bank::Bank;
use std::cell::RefCell;
const GAS_PRICE: u64 = 3;

const DEFAULT_COMITTMENT: Option<CommitmentConfig> = Some(CommitmentConfig {
    commitment: CommitmentLevel::Processed,
});

fn block_to_state_root(block: Option<BlockId>, meta: &JsonRpcRequestProcessor) -> Option<H256> {
    let block_id = block.unwrap_or_default();

    let mut found_block_hash = None;

    let block_num = match block_id {
        BlockId::Num(num) => num.0,
        BlockId::RelativeId(BlockRelId::Pending) | BlockId::RelativeId(BlockRelId::Latest) => {
            meta.get_last_available_evm_block().unwrap_or_else(|| {
                let bank = meta.bank(Some(CommitmentConfig::processed()));
                let evm = bank.evm_state.read().unwrap();
                evm.block_number().saturating_sub(1)
            })
        }
        BlockId::RelativeId(BlockRelId::Earliest) => meta.get_frist_available_evm_block(),
        BlockId::BlockHash { block_hash } => {
            found_block_hash = Some(block_hash.0);
            meta.get_evm_block_id_by_hash(block_hash.0)?
        }
    };
    meta.get_evm_block_by_id(block_num) // TODO: don't request full block.
        .filter(|(b, _)| {
            // if requested specific block hash, check that block with this hash is not in reorged fork
            found_block_hash
                .map(|block_hash| b.header.hash() == block_hash)
                .unwrap_or(true)
        })
        .map(|(b, _)| b.header.state_root)
}

fn block_parse_confirmed_num(
    block: Option<BlockId>,
    meta: &JsonRpcRequestProcessor,
) -> Option<u64> {
    let block = block?;
    match block {
        BlockId::BlockHash { .. } => return None,
        BlockId::RelativeId(BlockRelId::Earliest) => Some(meta.get_frist_available_evm_block()),
        BlockId::RelativeId(BlockRelId::Pending) | BlockId::RelativeId(BlockRelId::Latest) => {
            Some(meta.get_last_available_evm_block().unwrap_or_else(|| {
                let bank = meta.bank(Some(CommitmentConfig::processed()));
                let evm = bank.evm_state.read().unwrap();
                evm.block_number().saturating_sub(1)
            }))
        }

        BlockId::Num(num) => Some(num.0),
    }
}

pub struct ChainMockErpcImpl;
impl ChainMockERPC for ChainMockErpcImpl {
    type Metadata = JsonRpcRequestProcessor;

    fn network_id(&self, meta: Self::Metadata) -> Result<String, Error> {
        let bank = meta.bank(None);
        Ok(format!("{:#x}", bank.evm_chain_id))
    }

    fn chain_id(&self, meta: Self::Metadata) -> Result<Hex<u64>, Error> {
        let bank = meta.bank(None);
        Ok(Hex(bank.evm_chain_id))
    }

    // TODO: Add network info
    fn is_listening(&self, _meta: Self::Metadata) -> Result<bool, Error> {
        Ok(true)
    }

    fn peer_count(&self, _meta: Self::Metadata) -> Result<Hex<usize>, Error> {
        Ok(Hex(0))
    }

    fn sha3(&self, _meta: Self::Metadata, bytes: Bytes) -> Result<Hex<H256>, Error> {
        Ok(Hex(H256::from_slice(
            Keccak256::digest(bytes.0.as_slice()).as_slice(),
        )))
    }

    fn client_version(&self, _meta: Self::Metadata) -> Result<String, Error> {
        Ok(String::from("velas-chain/v0.3.0"))
    }

    fn protocol_version(&self, _meta: Self::Metadata) -> Result<String, Error> {
        Ok(String::from("0"))
    }

    fn is_syncing(&self, _meta: Self::Metadata) -> Result<bool, Error> {
        Err(Error::Unimplemented {})
    }

    fn coinbase(&self, _meta: Self::Metadata) -> Result<Hex<Address>, Error> {
        Ok(Hex(Address::from_low_u64_be(0)))
    }

    fn is_mining(&self, _meta: Self::Metadata) -> Result<bool, Error> {
        Ok(false)
    }

    fn hashrate(&self, _meta: Self::Metadata) -> Result<String, Error> {
        Err(Error::Unimplemented {})
    }

    fn block_transaction_count_by_number(
        &self,
        _meta: Self::Metadata,
        _block: String,
    ) -> Result<Option<Hex<usize>>, Error> {
        Ok(None)
    }

    fn block_transaction_count_by_hash(
        &self,
        _meta: Self::Metadata,
        _block_hash: Hex<H256>,
    ) -> Result<Option<Hex<usize>>, Error> {
        Err(Error::Unimplemented {})
    }

    fn uncle_by_block_hash_and_index(
        &self,
        _meta: Self::Metadata,
        _block_hash: Hex<H256>,
        _uncle_id: Hex<U256>,
    ) -> Result<Option<RPCBlock>, Error> {
        Err(Error::Unimplemented {})
    }

    fn uncle_by_block_number_and_index(
        &self,
        _meta: Self::Metadata,
        _block: String,
        _uncle_id: Hex<U256>,
    ) -> Result<Option<RPCBlock>, Error> {
        Err(Error::Unimplemented {})
    }

    fn block_uncles_count_by_hash(
        &self,
        _meta: Self::Metadata,
        _block_hash: Hex<H256>,
    ) -> Result<Option<Hex<usize>>, Error> {
        Err(Error::Unimplemented {})
    }

    fn block_uncles_count_by_number(
        &self,
        _meta: Self::Metadata,
        _block: String,
    ) -> Result<Option<Hex<usize>>, Error> {
        Err(Error::Unimplemented {})
    }

    fn transaction_by_block_hash_and_index(
        &self,
        _meta: Self::Metadata,
        _block_hash: Hex<H256>,
        _tx_id: Hex<U256>,
    ) -> Result<Option<RPCTransaction>, Error> {
        Err(Error::Unimplemented {})
    }

    fn transaction_by_block_number_and_index(
        &self,
        _meta: Self::Metadata,
        _block: String,
        _tx_id: Hex<U256>,
    ) -> Result<Option<RPCTransaction>, Error> {
        Err(Error::Unimplemented {})
    }
}

pub struct BasicErpcImpl;
impl BasicERPC for BasicErpcImpl {
    type Metadata = JsonRpcRequestProcessor;

    fn block_number(&self, meta: Self::Metadata) -> Result<Hex<usize>, Error> {
        let block = block_parse_confirmed_num(None, &meta).unwrap_or(0);
        Ok(Hex(block as usize))
    }

    fn balance(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        block: Option<BlockId>,
    ) -> Result<Hex<U256>, Error> {
        let root = block_to_state_root(block, &meta).ok_or(Error::BlockNotFound {
            block: block.unwrap_or_default(),
        })?;
        let bank = meta.bank(Some(CommitmentConfig::processed()));
        let evm_state = bank.evm_state.read().expect("Evm state poisoned");
        let account = evm_state
            .get_account_state_at(root, address.0)
            .unwrap_or_default();
        Ok(Hex(account.balance))
    }

    fn storage_at(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        data: Hex<H256>,
        block: Option<BlockId>,
    ) -> Result<Hex<H256>, Error> {
        let root = block_to_state_root(block, &meta).ok_or(Error::BlockNotFound {
            block: block.unwrap_or_default(),
        })?;
        let bank = meta.bank(Some(CommitmentConfig::processed()));
        let evm_state = bank.evm_state.read().expect("Evm state poisoned");
        Ok(Hex(evm_state
            .get_storage_at(root, address.0, data.0)
            .unwrap_or_default()))
    }

    fn transaction_count(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        block: Option<BlockId>,
    ) -> Result<Hex<U256>, Error> {
        let root = block_to_state_root(block, &meta).ok_or(Error::BlockNotFound {
            block: block.unwrap_or_default(),
        })?;
        let bank = meta.bank(Some(CommitmentConfig::processed()));
        let evm_state = bank.evm_state.read().expect("Evm state poisoned");
        let account = evm_state
            .get_account_state_at(root, address.0)
            .unwrap_or_default();
        Ok(Hex(account.nonce))
    }

    fn code(
        &self,
        meta: Self::Metadata,
        address: Hex<Address>,
        block: Option<BlockId>,
    ) -> Result<Bytes, Error> {
        let root = block_to_state_root(block, &meta).ok_or(Error::BlockNotFound {
            block: block.unwrap_or_default(),
        })?;
        let bank = meta.bank(Some(CommitmentConfig::processed()));
        let evm_state = bank.evm_state.read().expect("Evm state poisoned");
        let account = evm_state
            .get_account_state_at(root, address.0)
            .unwrap_or_default();
        Ok(Bytes(account.code.into()))
    }

    fn block_by_hash(
        &self,
        meta: Self::Metadata,
        block_hash: Hex<H256>,
        full: bool,
    ) -> Result<Option<RPCBlock>, Error> {
        debug!("Requested hash = {:?}", block_hash.0);
        let block = match meta.get_evm_block_id_by_hash(block_hash.0) {
            None => {
                error!("Not found block for hash:{}", block_hash);
                return Ok(None);
            }
            Some(b) => match meta.get_evm_block_by_id(b) {
                // check that found block only in valid fork.
                Some(block) if block.0.header.hash() == block_hash.0 => b,
                _ => return Ok(None),
            },
        };
        debug!("Found block = {:?}", block);

        self.block_by_number(meta, block.into(), full)
    }

    fn block_by_number(
        &self,
        meta: Self::Metadata,
        block: BlockId,
        full: bool,
    ) -> Result<Option<RPCBlock>, Error> {
        let num = block_parse_confirmed_num(Some(block), &meta);
        // TODO: Inline evm_state lookups, and request only solana headers.
        let (block, confirmed) = match num.and_then(|block_num| meta.get_evm_block_by_id(block_num))
        {
            None => {
                error!("Error requesting block:{:?} ({:?}) not found", block, num);
                return Ok(None);
            }
            Some(b) => b,
        };

        let bank = meta.bank(None);
        let chain_id = bank.evm_chain_id;

        let block_hash = block.header.hash();
        let transactions = if full {
            let txs = block
                .transactions
                .into_iter()
                .filter_map(|(hash, receipt)| {
                    RPCTransaction::new_from_receipt(receipt, hash, block_hash, chain_id).ok()
                })
                .collect();
            Either::Right(txs)
        } else {
            let txs = block
                .transactions
                .into_iter()
                .map(|(k, _v)| Hex(k))
                .collect();
            Either::Left(txs)
        };

        Ok(Some(RPCBlock::new_from_head(
            block.header,
            confirmed,
            transactions,
        )))
    }

    fn transaction_by_hash(
        &self,
        meta: Self::Metadata,
        tx_hash: Hex<H256>,
    ) -> Result<Option<RPCTransaction>, Error> {
        let bank = meta.bank(None);
        let chain_id = bank.evm_chain_id;
        let receipt = meta.get_evm_receipt_by_hash(tx_hash.0);

        Ok(match receipt {
            Some(receipt) => {
                let (block, _) = meta.get_evm_block_by_id(receipt.block_number).ok_or({
                    Error::BlockNotFound {
                        block: receipt.block_number.into(),
                    }
                })?;
                let block_hash = block.header.hash();
                Some(RPCTransaction::new_from_receipt(
                    receipt, tx_hash.0, block_hash, chain_id,
                )?)
            }
            None => None,
        })
    }

    fn transaction_receipt(
        &self,
        meta: Self::Metadata,
        tx_hash: Hex<H256>,
    ) -> Result<Option<RPCReceipt>, Error> {
        let receipt = meta.get_evm_receipt_by_hash(tx_hash.0);
        Ok(match receipt {
            Some(receipt) => {
                let (block, _) = meta.get_evm_block_by_id(receipt.block_number).ok_or({
                    Error::BlockNotFound {
                        block: receipt.block_number.into(),
                    }
                })?;
                let block_hash = block.header.hash();
                Some(RPCReceipt::new_from_receipt(
                    receipt, tx_hash.0, block_hash,
                )?)
            }
            None => None,
        })
    }

    fn call(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
        block: Option<BlockId>,
        meta_keys: Option<Vec<String>>,
    ) -> Result<Bytes, Error> {
        let meta_keys: Result<Vec<_>, _> = meta_keys
            .into_iter()
            .flatten()
            .map(|s| solana_sdk::pubkey::Pubkey::from_str(&s))
            .collect();
        let saved_root = block_to_state_root(block, &meta).ok_or(Error::BlockNotFound {
            block: block.unwrap_or_default(),
        })?;
        let result = call(
            meta,
            tx,
            Some(saved_root),
            meta_keys.into_native_error(false)?,
        )?;
        Ok(Bytes(result.1))
    }

    fn gas_price(&self, _meta: Self::Metadata) -> Result<Hex<Gas>, Error> {
        Ok(Hex(
            solana_evm_loader_program::scope::evm::lamports_to_gwei(GAS_PRICE),
        ))
    }

    fn trace_call(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
        traces: Vec<String>, //TODO: check trace = ["trace"]
        block: Option<BlockId>,
        meta_info: Option<TraceMeta>,
    ) -> Result<evm_rpc::trace::TraceResultsWithTransactionHash, Error> {
        Ok(self
            .trace_call_many(meta, vec![(tx, traces, meta_info)], block)?
            .into_iter()
            .next()
            .expect("One item should be returned"))
    }

    fn trace_call_many(
        &self,
        meta: Self::Metadata,
        tx_traces: Vec<(RPCTransaction, Vec<String>, Option<TraceMeta>)>,
        block: Option<BlockId>,
    ) -> Result<Vec<evm_rpc::trace::TraceResultsWithTransactionHash>, Error> {
        let saved_root = block_to_state_root(block, &meta).ok_or(Error::BlockNotFound {
            block: block.unwrap_or_default(),
        })?;
        let mut txs = Vec::new();
        let mut txs_meta = Vec::new();

        // TODO: Handle Vec<String> - traces array, check that it contain "trace" string.
        for (t, _, meta) in tx_traces {
            let meta = meta.unwrap_or_default();
            let meta_keys: Result<Vec<_>, _> = meta
                .meta_keys
                .iter()
                .flatten()
                .map(|s| solana_sdk::pubkey::Pubkey::from_str(&s))
                .collect();

            txs.push((t, meta_keys.into_native_error(false)?));
            txs_meta.push(meta);
        }

        let traces = call_many(meta, &txs, Some(saved_root))?.into_iter();

        let mut result = Vec::new();
        for (trace, meta_tx) in traces.zip(txs_meta) {
            result.push(evm_rpc::trace::TraceResultsWithTransactionHash {
                trace: trace.3.into_iter().map(From::from).collect(),
                output: trace.1.into(),
                transaction_hash: meta_tx.transaction_hash.map(Hex),
                transaction_index: meta_tx.transaction_index.map(Hex),
                block_hash: meta_tx.block_hash.map(Hex),
                block_number: meta_tx.block_number.map(Hex),
            })
        }
        Ok(result)
    }

    fn trace_replay_transaction(
        &self,
        meta: Self::Metadata,
        tx_hash: Hex<H256>,
        traces: Vec<String>,
        meta_info: Option<TraceMeta>,
    ) -> Result<Option<evm_rpc::trace::TraceResultsWithTransactionHash>, Error> {
        let mut meta_info = meta_info.unwrap_or_default();
        let tx = self.transaction_by_hash(meta.clone(), tx_hash);
        match tx {
            Ok(Some(tx)) => {
                let block = if let Some(block) = tx.block_number {
                    block.0.as_u64().into()
                } else {
                    return Ok(None);
                };
                meta_info.transaction_hash = tx.hash.map(|v| v.0);
                meta_info.transaction_index = tx.transaction_index.map(|v| v.0);
                meta_info.block_number = tx.block_number.map(|v| v.0);
                meta_info.block_hash = tx.block_hash.map(|v| v.0);
                let result = self.trace_call(meta, tx, traces, Some(block), Some(meta_info))?;
                Ok(Some(result))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn trace_replay_block(
        &self,
        meta: Self::Metadata,
        block_num: BlockId,
        traces: Vec<String>,
        meta_info: Option<TraceMeta>,
    ) -> Result<Vec<evm_rpc::trace::TraceResultsWithTransactionHash>, Error> {
        let block =
            if let Some(block) = self.block_by_number(meta.clone(), block_num.clone(), true)? {
                block
            } else {
                return Err(Error::StateNotFoundForBlock { block: block_num });
            };
        let txs = match block.transactions {
            Either::Right(txs) => txs,
            _ => return Err(Error::Unimplemented {}),
        };
        let meta_info = meta_info.unwrap_or_default();
        let transactions = txs
            .into_iter()
            .map(|tx| {
                let mut meta_info = meta_info.clone();
                meta_info.transaction_hash = tx.hash.map(|v| v.0);
                meta_info.transaction_index = tx.transaction_index.map(|v| v.0);
                meta_info.block_number = tx.block_number.map(|v| v.0);
                meta_info.block_hash = tx.block_hash.map(|v| v.0);
                (tx, traces.clone(), Some(meta_info))
            })
            .collect();
        // execute on pervious block
        self.trace_call_many(
            meta,
            transactions,
            Some(block.number.as_u64().saturating_sub(1).into()),
        )
    }

    fn estimate_gas(
        &self,
        meta: Self::Metadata,
        tx: RPCTransaction,
        block: Option<BlockId>,
        meta_keys: Option<Vec<String>>,
    ) -> Result<Hex<Gas>, Error> {
        let meta_keys: Result<Vec<_>, _> = meta_keys
            .into_iter()
            .flatten()
            .map(|s| solana_sdk::pubkey::Pubkey::from_str(&s))
            .collect();
        let saved_root = block_to_state_root(block, &meta).ok_or(Error::BlockNotFound {
            block: block.unwrap_or_default(),
        })?;
        let result = call(
            meta,
            tx,
            Some(saved_root),
            meta_keys.into_native_error(false)?,
        )?;
        Ok(Hex(result.2.into()))
    }

    fn logs(&self, meta: Self::Metadata, log_filter: RPCLogFilter) -> Result<Vec<RPCLog>, Error> {
        const MAX_NUM_BLOCKS: u64 = 2000;
        let bank = meta.bank(None);

        let evm_lock = bank.evm_state.read().expect("Evm lock poisoned");
        let block_num = evm_lock.block_number();
        let to = block_parse_confirmed_num(log_filter.to_block, &meta).unwrap_or(block_num);
        let from = block_parse_confirmed_num(log_filter.from_block, &meta).unwrap_or(block_num);
        if to > from + MAX_NUM_BLOCKS {
            warn!(
                "Log filter, block range is too big, reducing, to={}, from={}",
                to, from
            );
            return Err(Error::InvalidBlocksRange {
                starting: from,
                ending: to,
                batch_size: Some(MAX_NUM_BLOCKS),
            });
        }

        let filter = LogFilter {
            address: log_filter.address.map(|k| k.0),
            topics: log_filter
                .topics
                .into_iter()
                .flatten()
                .map(RPCTopicFilter::into_topics)
                .collect(),
            from_block: from,
            to_block: to,
        };

        debug!("filter = {:?}", filter);

        let logs = meta
            .filter_logs(filter)
            .map_err(|e| {
                debug!("filter_logs error = {:?}", e);
                e
            })
            .into_native_error(false)?;
        Ok(logs.into_iter().map(|l| l.into()).collect())
    }
}

fn call(
    meta: JsonRpcRequestProcessor,
    tx: RPCTransaction,
    saved_root: Option<H256>,
    meta_keys: Vec<solana_sdk::pubkey::Pubkey>,
) -> Result<
    (
        evm_state::ExitSucceed,
        Vec<u8>,
        u64,
        Vec<evm_state::executor::Trace>,
    ),
    Error,
> {
    let result = call_many(meta, &[(tx, meta_keys)], saved_root)?;
    let (reason, data, gas_used, traces) = result
        .into_iter()
        .next()
        .expect("Should contain result for tx.");
    let (reason, data) = match reason {
        evm_state::ExitReason::Error(error) => Err(Error::CallError {
            data: data.into(),
            error,
        }),
        evm_state::ExitReason::Revert(error) => Err(Error::CallRevert {
            data: data.into(),
            error,
        }),
        evm_state::ExitReason::Fatal(error) => Err(Error::CallFatal { error }),
        evm_state::ExitReason::Succeed(s) => Ok((s, data)),
    }?;
    Ok((reason, data, gas_used, traces))
}

fn call_many(
    meta: JsonRpcRequestProcessor,
    txs: &[(RPCTransaction, Vec<solana_sdk::pubkey::Pubkey>)],
    saved_root: Option<H256>,
) -> Result<
    Vec<(
        evm_state::ExitReason,
        Vec<u8>,
        u64,
        Vec<evm_state::executor::Trace>,
    )>,
    Error,
> {
    let bank = meta.bank(DEFAULT_COMITTMENT);
    let evm_state = bank
        .evm_state
        .read()
        .expect("meta bank EVM state was poisoned");

    let evm_state = evm_state.clone();
    let evm_state = match evm_state.new_from_parent(bank.clock().unix_timestamp, false) {
        evm_state::EvmState::Incomming(i) => i,
        evm_state::EvmState::Committed(_) => unreachable!(),
    };
    let evm_state = if let Some(root) = saved_root {
        evm_state
            .new_incomming_for_root(root)
            .ok_or(Error::StateRootNotFound { state: root })?
    } else {
        evm_state
    };

    let estimate_config = evm_state::EvmConfig {
        estimate: true,
        ..Default::default()
    };

    let last_hashes = bank.evm_hashes();
    let mut executor = evm_state::Executor::with_config(
        evm_state,
        evm_state::ChainContext::new(last_hashes),
        estimate_config,
    );

    debug!("running evm executor = {:?}", executor);
    let mut result = Vec::new();
    for (tx, meta_keys) in txs {
        result.push(call_inner(
            &mut executor,
            tx.clone(),
            meta_keys.clone(),
            &*bank,
        )?)
    }
    Ok(result)
}

fn call_inner(
    executor: &mut evm_state::Executor,
    tx: RPCTransaction,
    meta_keys: Vec<solana_sdk::pubkey::Pubkey>,
    bank: &Bank,
) -> Result<
    (
        evm_state::ExitReason,
        Vec<u8>,
        u64,
        Vec<evm_state::executor::Trace>,
    ),
    Error,
> {
    let caller = tx.from.map(|a| a.0).unwrap_or_default();

    let value = tx.value.map(|a| a.0).unwrap_or_else(|| 0.into());
    let input = tx.input.map(|a| a.0).unwrap_or_else(Vec::new);
    let gas_limit = tx.gas.map(|a| a.0).unwrap_or_else(|| u64::MAX.into());
    // On estimate set gas price to zero, to avoid out of funds errors.
    let gas_price = u64::MIN.into();

    let nonce = tx
        .nonce
        .map(|a| a.0)
        .unwrap_or_else(|| executor.nonce(caller));
    let tx_chain_id = executor.chain_id();
    let tx_hash = tx.hash.map(|a| a.0).unwrap_or_else(|| H256::random());

    let evm_state_balance = bank
        .get_account(&solana_sdk::evm_state::id())
        .unwrap_or_default()
        .lamports;

    let (user_accounts, action) = if let Some(address) = tx.to {
        use solana_evm_loader_program::precompiles::*;
        let address = address.0;
        debug!(
            "Trying to execute tx = {:?}",
            (caller, address, value, &input, gas_limit)
        );

        let mut meta_keys: Vec<_> = meta_keys
            .into_iter()
            .map(|pk| {
                let user_account = RefCell::new(bank.get_account(&pk).unwrap_or_default());
                (user_account, pk)
            })
            .collect();

        // Shortcut for swap tokens to native, will add solana account to transaction.
        if address == *ETH_TO_VLX_ADDR {
            debug!("Found transferToNative transaction");
            match ETH_TO_VLX_CODE.parse_abi(&input) {
                Ok(pk) => {
                    info!("Adding account to meta = {}", pk);

                    let user_account = RefCell::new(bank.get_account(&pk).unwrap_or_default());
                    meta_keys.push((user_account, pk))
                }
                Err(e) => {
                    error!("Error in parsing abi = {}", e);
                }
            }
        }

        (meta_keys, TransactionAction::Call(address))
    } else {
        (vec![], TransactionAction::Create)
    };
    let user_accounts: Vec<_> = user_accounts
        .iter()
        .map(|(user_account, pk)| KeyedAccount::new(pk, false, user_account))
        .collect();
    let result = executor
        .transaction_execute_raw(
            caller,
            nonce,
            gas_price,
            gas_limit,
            action,
            input,
            value,
            Some(tx_chain_id),
            tx_hash,
            solana_evm_loader_program::precompiles::simulation_entrypoint(
                executor.support_precompile(),
                evm_state_balance,
                &user_accounts,
            ),
        )
        .with_context(|| EvmStateError)?;
    Ok((
        result.exit_reason,
        result.exit_data,
        result.used_gas,
        result.traces,
    ))
}
