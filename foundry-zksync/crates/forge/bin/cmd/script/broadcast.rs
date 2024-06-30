use super::{
    multi::MultiChainSequence,
    providers::ProvidersManager,
    receipts::clear_pendings,
    sequence::ScriptSequence,
    transaction::{TransactionWithMetadata, ZkTransaction},
    verify::VerifyBundle,
    NestedValue, ScriptArgs, ScriptConfig, ScriptResult,
};
use alloy_primitives::{utils::format_units, Address, TxHash, U256};
use ethers_core::types::transaction::eip2718::TypedTransaction;
use ethers_providers::{JsonRpcClient, Middleware, Provider};
use ethers_signers::Signer;
use eyre::{bail, Context, ContextCompat, Result};
use forge::{inspectors::cheatcodes::BroadcastableTransactions, traces::CallTraceDecoder};
use foundry_cli::{
    init_progress, update_progress,
    utils::{has_batch_support, has_different_gas_calc},
};
use foundry_common::{
    provider::{
        alloy::RpcUrl,
        ethers::{estimate_eip1559_fees, try_get_http_provider, RetryProvider},
    },
    shell,
    types::{ToAlloy, ToEthers},
    ContractsByArtifact,
};
use foundry_compilers::{artifacts::Libraries, ArtifactId};
use foundry_config::Config;
use foundry_wallets::WalletSigner;
use foundry_zksync_compiler::DualCompiledContracts;
use futures::StreamExt;
use std::{
    cmp::min,
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};
use zksync_web3_rs::{
    eip712::{Eip712Meta, Eip712Transaction, Eip712TransactionRequest},
    zks_utils::EIP712_TX_TYPE,
};

impl ScriptArgs {
    /// Sends the transactions which haven't been broadcasted yet.
    pub async fn send_transactions(
        &self,
        deployment_sequence: &mut ScriptSequence,
        fork_url: &str,
        signers: &HashMap<Address, WalletSigner>,
    ) -> Result<()> {
        let provider = Arc::new(try_get_http_provider(fork_url)?);
        let already_broadcasted = deployment_sequence.receipts.len();

        if already_broadcasted < deployment_sequence.transactions.len() {
            let required_addresses: HashSet<Address> = deployment_sequence
                .typed_transactions()
                .skip(already_broadcasted)
                .map(|tx| (*tx.from().expect("No sender for onchain transaction!")).to_alloy())
                .collect();

            let (send_kind, chain) = if self.unlocked {
                let chain = provider.get_chainid().await?;
                let mut senders = HashSet::from([self
                    .evm_opts
                    .sender
                    .wrap_err("--sender must be set with --unlocked")?]);
                // also take all additional senders that where set manually via broadcast
                senders.extend(
                    deployment_sequence
                        .typed_transactions()
                        .filter_map(|tx| tx.from().copied().map(|addr| addr.to_alloy())),
                );
                (SendTransactionsKind::Unlocked(senders), chain.as_u64())
            } else {
                let mut missing_addresses = Vec::new();

                println!("\n###\nFinding wallets for all the necessary addresses...");
                for addr in &required_addresses {
                    if !signers.contains_key(addr) {
                        missing_addresses.push(addr);
                    }
                }

                if !missing_addresses.is_empty() {
                    let mut error_msg = String::new();

                    // This is an actual used address
                    if required_addresses.contains(&Config::DEFAULT_SENDER) {
                        error_msg += "\nYou seem to be using Foundry's default sender. Be sure to set your own --sender.\n";
                    }

                    eyre::bail!(
                        "{}No associated wallet for addresses: {:?}. Unlocked wallets: {:?}",
                        error_msg,
                        missing_addresses,
                        signers.keys().collect::<Vec<_>>()
                    );
                }

                let chain = provider.get_chainid().await?.as_u64();

                (SendTransactionsKind::Raw(signers), chain)
            };

            // We only wait for a transaction receipt before sending the next transaction, if there
            // is more than one signer. There would be no way of assuring their order
            // otherwise. Or if the chain does not support batched transactions (eg. Arbitrum).
            let sequential_broadcast =
                send_kind.signers_count() != 1 || self.slow || !has_batch_support(chain);

            // Make a one-time gas price estimation
            let (gas_price, eip1559_fees) = {
                match deployment_sequence.transactions.front().unwrap().typed_tx() {
                    TypedTransaction::Eip1559(_) => {
                        let fees = estimate_eip1559_fees(&provider, Some(chain))
                            .await
                            .wrap_err("Failed to estimate EIP1559 fees. This chain might not support EIP1559, try adding --legacy to your command.")?;

                        (None, Some(fees))
                    }
                    _ => (provider.get_gas_price().await.ok(), None),
                }
            };

            // Iterate through transactions, matching the `from` field with the associated
            // wallet. Then send the transaction. Panics if we find a unknown `from`
            let sequence = deployment_sequence
                .transactions
                .iter()
                .skip(already_broadcasted)
                .map(|tx_with_metadata| {
                    let tx = tx_with_metadata.typed_tx();
                    let from = (*tx.from().expect("No sender for onchain transaction!")).to_alloy();

                    let kind = send_kind.for_sender(&from)?;
                    let is_fixed_gas_limit = tx_with_metadata.is_fixed_gas_limit;
                    let zk = tx_with_metadata.zk.clone();

                    let mut tx = tx.clone();

                    tx.set_chain_id(chain);

                    if let Some(gas_price) = self.with_gas_price {
                        tx.set_gas_price(gas_price.to_ethers());
                    } else {
                        // fill gas price
                        match tx {
                            TypedTransaction::Eip1559(ref mut inner) => {
                                let eip1559_fees =
                                    eip1559_fees.expect("Could not get eip1559 fee estimation.");
                                if let Some(priority_gas_price) = self.priority_gas_price {
                                    inner.max_priority_fee_per_gas =
                                        Some(priority_gas_price.to_ethers());
                                } else {
                                    inner.max_priority_fee_per_gas = Some(eip1559_fees.1);
                                }
                                inner.max_fee_per_gas = Some(eip1559_fees.0);
                            }
                            _ => {
                                tx.set_gas_price(gas_price.expect("Could not get gas_price."));
                            }
                        }
                    }

                    Ok((tx, zk, kind, is_fixed_gas_limit))
                })
                .collect::<Result<Vec<_>>>()?;

            let pb = init_progress!(deployment_sequence.transactions, "txes");

            // We send transactions and wait for receipts in batches of 100, since some networks
            // cannot handle more than that.
            let batch_size = 100;
            let mut index = 0;

            for (batch_number, batch) in sequence.chunks(batch_size).map(|f| f.to_vec()).enumerate()
            {
                let mut pending_transactions = vec![];

                shell::println(format!(
                    "##\nSending transactions [{} - {}].",
                    batch_number * batch_size,
                    batch_number * batch_size + min(batch_size, batch.len()) - 1
                ))?;
                for (tx, zk, kind, is_fixed_gas_limit) in batch.into_iter() {
                    let tx_hash = self.send_transaction(
                        provider.clone(),
                        tx,
                        zk,
                        kind,
                        sequential_broadcast,
                        fork_url,
                        is_fixed_gas_limit,
                    );

                    if sequential_broadcast {
                        let tx_hash = tx_hash.await?;
                        deployment_sequence.add_pending(index, tx_hash);

                        update_progress!(pb, (index + already_broadcasted));
                        index += 1;

                        clear_pendings(provider.clone(), deployment_sequence, Some(vec![tx_hash]))
                            .await?;
                    } else {
                        pending_transactions.push(tx_hash);
                    }
                }

                if !pending_transactions.is_empty() {
                    let mut buffer = futures::stream::iter(pending_transactions).buffered(7);

                    while let Some(tx_hash) = buffer.next().await {
                        let tx_hash = tx_hash?;
                        deployment_sequence.add_pending(index, tx_hash);

                        update_progress!(pb, (index + already_broadcasted));
                        index += 1;
                    }

                    // Checkpoint save
                    deployment_sequence.save()?;

                    if !sequential_broadcast {
                        shell::println("##\nWaiting for receipts.")?;
                        clear_pendings(provider.clone(), deployment_sequence, None).await?;
                    }
                }

                // Checkpoint save
                deployment_sequence.save()?;
            }
        }

        shell::println("\n\n==========================")?;
        shell::println("\nONCHAIN EXECUTION COMPLETE & SUCCESSFUL.")?;

        let (total_gas, total_gas_price, total_paid) = deployment_sequence.receipts.iter().fold(
            (U256::ZERO, U256::ZERO, U256::ZERO),
            |acc, receipt| {
                let gas_used = receipt.gas_used.unwrap_or_default().to_alloy();
                let gas_price = receipt.effective_gas_price.unwrap_or_default().to_alloy();
                (acc.0 + gas_used, acc.1 + gas_price, acc.2 + gas_used * gas_price)
            },
        );
        let paid = format_units(total_paid, 18).unwrap_or_else(|_| "N/A".to_string());
        let avg_gas_price =
            format_units(total_gas_price / U256::from(deployment_sequence.receipts.len()), 9)
                .unwrap_or_else(|_| "N/A".to_string());
        shell::println(format!(
            "Total Paid: {} ETH ({} gas * avg {} gwei)",
            paid.trim_end_matches('0'),
            total_gas,
            avg_gas_price.trim_end_matches('0').trim_end_matches('.')
        ))?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_transaction(
        &self,
        provider: Arc<RetryProvider>,
        mut tx: TypedTransaction,
        zk: Option<ZkTransaction>,
        kind: SendTransactionKind<'_>,
        sequential_broadcast: bool,
        fork_url: &str,
        is_fixed_gas_limit: bool,
    ) -> Result<TxHash> {
        let from = tx.from().expect("no sender");

        if sequential_broadcast {
            let nonce = forge::next_nonce((*from).to_alloy(), fork_url, None)
                .await
                .map_err(|_| eyre::eyre!("Not able to query the EOA nonce."))?;

            let tx_nonce = tx.nonce().expect("no nonce");
            if let Ok(tx_nonce) = u64::try_from(tx_nonce.to_alloy()) {
                if nonce != tx_nonce {
                    bail!("EOA nonce changed unexpectedly while sending transactions. Expected {tx_nonce} got {nonce} from provider.")
                }
            }
        }

        match kind {
            SendTransactionKind::Unlocked(addr) => {
                debug!("sending transaction from unlocked account {:?}: {:?}", addr, tx);

                // Chains which use `eth_estimateGas` are being sent sequentially and require their
                // gas to be re-estimated right before broadcasting.
                if !is_fixed_gas_limit &&
                    (has_different_gas_calc(provider.get_chainid().await?.as_u64()) ||
                        self.skip_simulation)
                {
                    self.estimate_gas(&mut tx, &provider).await?;
                }

                // Submit the transaction
                let pending = provider.send_transaction(tx, None).await?;

                Ok(pending.tx_hash().to_alloy())
            }
            SendTransactionKind::Raw(signer) => self.broadcast(provider, signer, tx, zk).await,
        }
    }

    /// Executes the created transactions, and if no error has occurred, broadcasts
    /// them.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_broadcastable_transactions(
        &self,
        mut result: ScriptResult,
        libraries: Libraries,
        decoder: &CallTraceDecoder,
        mut script_config: ScriptConfig,
        verify: VerifyBundle,
        signers: &HashMap<Address, WalletSigner>,
        dual_compiled_contracts: Option<DualCompiledContracts>,
    ) -> Result<()> {
        if let Some(txs) = result.transactions.take() {
            script_config.collect_rpcs(&txs);
            script_config.check_multi_chain_constraints(&libraries)?;
            script_config.check_shanghai_support().await?;

            if !script_config.missing_rpc {
                trace!(target: "script", "creating deployments");

                let mut deployments = self
                    .create_script_sequences(
                        txs,
                        &result,
                        &mut script_config,
                        decoder,
                        &verify.known_contracts,
                        dual_compiled_contracts,
                    )
                    .await?;

                if script_config.has_multiple_rpcs() {
                    trace!(target: "script", "broadcasting multi chain deployment");

                    let multi = MultiChainSequence::new(
                        deployments.clone(),
                        &self.sig,
                        script_config.target_contract(),
                        &script_config.config,
                        self.broadcast,
                    )?;

                    if self.broadcast {
                        self.multi_chain_deployment(
                            multi,
                            libraries,
                            &script_config.config,
                            verify,
                            signers,
                        )
                        .await?;
                    }
                } else if self.broadcast {
                    self.single_deployment(
                        deployments.first_mut().expect("missing deployment"),
                        script_config,
                        libraries,
                        verify,
                        signers,
                    )
                    .await?;
                }

                if !self.broadcast {
                    shell::println("\nSIMULATION COMPLETE. To broadcast these transactions, add --broadcast and wallet configuration(s) to the previous command. See forge script --help for more.")?;
                }
            } else {
                shell::println("\nIf you wish to simulate on-chain transactions pass a RPC URL.")?;
            }
        }
        Ok(())
    }

    /// Broadcasts a single chain script.
    async fn single_deployment(
        &self,
        deployment_sequence: &mut ScriptSequence,
        script_config: ScriptConfig,
        libraries: Libraries,
        verify: VerifyBundle,
        signers: &HashMap<Address, WalletSigner>,
    ) -> Result<()> {
        trace!(target: "script", "broadcasting single chain deployment");

        if self.verify {
            deployment_sequence.verify_preflight_check(&script_config.config, &verify)?;
        }

        let rpc = script_config.total_rpcs.into_iter().next().expect("exists; qed");

        deployment_sequence.add_libraries(libraries);

        self.send_transactions(deployment_sequence, &rpc, signers).await?;

        if self.verify {
            return deployment_sequence.verify_contracts(&script_config.config, verify).await;
        }
        Ok(())
    }

    /// Given the collected transactions it creates a list of [`ScriptSequence`].  List length will
    /// be higher than 1, if we're dealing with a multi chain deployment.
    ///
    /// If `--skip-simulation` is not passed, it will make an onchain simulation of the transactions
    /// before adding them to [`ScriptSequence`].
    async fn create_script_sequences(
        &self,
        txs: BroadcastableTransactions,
        script_result: &ScriptResult,
        script_config: &mut ScriptConfig,
        decoder: &CallTraceDecoder,
        known_contracts: &ContractsByArtifact,
        dual_compiled_contracts: Option<DualCompiledContracts>,
    ) -> Result<Vec<ScriptSequence>> {
        if !txs.is_empty() {
            let gas_filled_txs = self
                .fills_transactions_with_gas(
                    txs,
                    script_config,
                    decoder,
                    known_contracts,
                    dual_compiled_contracts,
                )
                .await?;

            let returns = self.get_returns(&*script_config, &script_result.returned)?;

            return self
                .bundle_transactions(
                    gas_filled_txs,
                    &script_config.target_contract().clone(),
                    &mut script_config.config,
                    returns,
                )
                .await;
        } else if self.broadcast {
            eyre::bail!("No onchain transactions generated in script");
        }

        Ok(vec![])
    }

    /// Takes the collected transactions and executes them locally before converting them to
    /// [`TransactionWithMetadata`] with the appropriate gas execution estimation. If
    /// `--skip-simulation` is passed, then it will skip the execution.
    async fn fills_transactions_with_gas(
        &self,
        txs: BroadcastableTransactions,
        script_config: &ScriptConfig,
        decoder: &CallTraceDecoder,
        known_contracts: &ContractsByArtifact,
        dual_compiled_contracts: Option<DualCompiledContracts>,
    ) -> Result<VecDeque<TransactionWithMetadata>> {
        let gas_filled_txs = if self.skip_simulation {
            shell::println("\nSKIPPING ON CHAIN SIMULATION.")?;
            txs.into_iter()
                .map(|btx| {
                    let mut tx = TransactionWithMetadata::from_zk_tx_request(
                        btx.transaction,
                        btx.zk_tx.map(|zk_tx| ZkTransaction { factory_deps: zk_tx.factory_deps }),
                    );
                    tx.rpc = btx.rpc;
                    tx
                })
                .collect()
        } else {
            self.onchain_simulation(
                txs,
                script_config,
                decoder,
                known_contracts,
                dual_compiled_contracts,
            )
            .await
            .wrap_err("\nTransaction failed when running the on-chain simulation. Check the trace above for more information.")?
        };
        Ok(gas_filled_txs)
    }

    /// Returns all transactions of the [`TransactionWithMetadata`] type in a list of
    /// [`ScriptSequence`]. List length will be higher than 1, if we're dealing with a multi
    /// chain deployment.
    ///
    /// Each transaction will be added with the correct transaction type and gas estimation.
    async fn bundle_transactions(
        &self,
        transactions: VecDeque<TransactionWithMetadata>,
        target: &ArtifactId,
        config: &mut Config,
        returns: HashMap<String, NestedValue>,
    ) -> Result<Vec<ScriptSequence>> {
        // User might be using both "in-code" forks and `--fork-url`.
        let last_rpc = &transactions.back().expect("exists; qed").rpc;
        let is_multi_deployment = transactions.iter().any(|tx| &tx.rpc != last_rpc);

        let mut total_gas_per_rpc: HashMap<RpcUrl, U256> = HashMap::new();

        // Batches sequence of transactions from different rpcs.
        let mut new_sequence = VecDeque::new();
        let mut manager = ProvidersManager::default();
        let mut deployments = vec![];

        // Config is used to initialize the sequence chain, so we need to change when handling a new
        // sequence. This makes sure we don't lose the original value.
        let original_config_chain = config.chain;

        // Peeking is used to check if the next rpc url is different. If so, it creates a
        // [`ScriptSequence`] from all the collected transactions up to this point.
        let mut txes_iter = transactions.into_iter().peekable();

        while let Some(mut tx) = txes_iter.next() {
            let tx_rpc = match tx.rpc.clone() {
                Some(rpc) => rpc,
                None => {
                    let rpc = self.evm_opts.ensure_fork_url()?.clone();
                    // Fills the RPC inside the transaction, if missing one.
                    tx.rpc = Some(rpc.clone());
                    rpc
                }
            };

            let provider_info = manager.get_or_init_provider(&tx_rpc, self.legacy).await?;

            // Handles chain specific requirements.
            tx.change_type(provider_info.is_legacy);
            tx.transaction.set_chain_id(provider_info.chain);

            if !self.skip_simulation {
                let typed_tx = tx.typed_tx_mut();

                if has_different_gas_calc(provider_info.chain) {
                    trace!("estimating with different gas calculation");
                    let gas = *typed_tx.gas().expect("gas is set by simulation.");

                    // We are trying to show the user an estimation of the total gas usage.
                    //
                    // However, some transactions might depend on previous ones. For
                    // example, tx1 might deploy a contract that tx2 uses. That
                    // will result in the following `estimate_gas` call to fail,
                    // since tx1 hasn't been broadcasted yet.
                    //
                    // Not exiting here will not be a problem when actually broadcasting, because
                    // for chains where `has_different_gas_calc` returns true,
                    // we await each transaction before broadcasting the next
                    // one.
                    if let Err(err) = self.estimate_gas(typed_tx, &provider_info.provider).await {
                        trace!("gas estimation failed: {err}");

                        // Restore gas value, since `estimate_gas` will remove it.
                        typed_tx.set_gas(gas);
                    }
                }

                let total_gas = total_gas_per_rpc.entry(tx_rpc.clone()).or_insert(U256::ZERO);
                *total_gas += (*typed_tx.gas().expect("gas is set")).to_alloy();
            }

            new_sequence.push_back(tx);
            // We only create a [`ScriptSequence`] object when we collect all the rpc related
            // transactions.
            if let Some(next_tx) = txes_iter.peek() {
                if next_tx.rpc == Some(tx_rpc) {
                    continue;
                }
            }

            config.chain = Some(provider_info.chain.into());
            let sequence = ScriptSequence::new(
                new_sequence,
                returns.clone(),
                &self.sig,
                target,
                config,
                self.broadcast,
                is_multi_deployment,
            )?;

            deployments.push(sequence);

            new_sequence = VecDeque::new();
        }

        // Restore previous config chain.
        config.chain = original_config_chain;

        if !self.skip_simulation {
            // Present gas information on a per RPC basis.
            for (rpc, total_gas) in total_gas_per_rpc {
                let provider_info = manager.get(&rpc).expect("provider is set.");

                // We don't store it in the transactions, since we want the most updated value.
                // Right before broadcasting.
                let per_gas = if let Some(gas_price) = self.with_gas_price {
                    gas_price
                } else {
                    provider_info.gas_price()?
                };

                shell::println("\n==========================")?;
                shell::println(format!("\nChain {}", provider_info.chain))?;

                shell::println(format!(
                    "\nEstimated gas price: {} gwei",
                    format_units(per_gas, 9)
                        .unwrap_or_else(|_| "[Could not calculate]".to_string())
                        .trim_end_matches('0')
                        .trim_end_matches('.')
                ))?;
                shell::println(format!("\nEstimated total gas used for script: {total_gas}"))?;
                shell::println(format!(
                    "\nEstimated amount required: {} ETH",
                    format_units(total_gas.saturating_mul(per_gas), 18)
                        .unwrap_or_else(|_| "[Could not calculate]".to_string())
                        .trim_end_matches('0')
                ))?;
                shell::println("\n==========================")?;
            }
        }
        Ok(deployments)
    }

    /// Uses the signer to submit a transaction to the network. If it fails, it tries to retrieve
    /// the transaction hash that can be used on a later run with `--resume`.
    async fn broadcast(
        &self,
        provider: Arc<RetryProvider>,
        signer: &WalletSigner,
        mut legacy_or_1559: TypedTransaction,
        zk: Option<ZkTransaction>,
    ) -> Result<TxHash> {
        debug!("sending transaction: {:?}", legacy_or_1559);

        // Chains which use `eth_estimateGas` are being sent sequentially and require their gas
        // to be re-estimated right before broadcasting.
        if has_different_gas_calc(signer.chain_id()) || self.skip_simulation {
            // if already set, some RPC endpoints might simply return the gas value that is
            // already set in the request and omit the estimate altogether, so
            // we remove it here
            let _ = legacy_or_1559.gas_mut().take();

            self.estimate_gas(&mut legacy_or_1559, &provider).await?;
        }

        let signed_tx = if let Some(zk) = zk {
            let custom_data = Eip712Meta::new().factory_deps(zk.factory_deps);

            let mut deploy_request = Eip712TransactionRequest::new()
                .r#type(EIP712_TX_TYPE)
                .from(*legacy_or_1559.from().unwrap())
                .to(*legacy_or_1559.to().and_then(|to| to.as_address()).unwrap())
                .chain_id(legacy_or_1559.chain_id().unwrap().as_u64())
                .nonce(legacy_or_1559.nonce().unwrap())
                .gas_price(legacy_or_1559.gas_price().unwrap())
                .max_fee_per_gas(legacy_or_1559.max_cost().unwrap())
                .data(legacy_or_1559.data().cloned().unwrap())
                .custom_data(custom_data);

            let gas_price = provider.get_gas_price().await?;
            let fee: zksync_web3_rs::zks_provider::types::Fee =
                provider.request("zks_estimateFee", [deploy_request.clone()]).await.unwrap();
            deploy_request = deploy_request
                .gas_limit(fee.gas_limit)
                .max_fee_per_gas(fee.max_fee_per_gas)
                .max_priority_fee_per_gas(fee.max_priority_fee_per_gas)
                .gas_price(gas_price);

            let signable: Eip712Transaction =
                deploy_request.clone().try_into().expect("converting deploy request");
            debug!("sending transaction: {:?}", signable);

            let signature =
                signer.sign_typed_data(&signable).await.wrap_err("Failed to sign typed data")?;

            let encoded_rlp =
                &*deploy_request.rlp_signed(signature).expect("able to rlp encode deploy request");
            [&[EIP712_TX_TYPE], encoded_rlp].concat().into()
        } else {
            // Signing manually so we skip `fill_transaction` and its `eth_createAccessList`
            // request.
            let signature = signer
                .sign_transaction(&legacy_or_1559)
                .await
                .wrap_err("Failed to sign transaction")?;
            legacy_or_1559.rlp_signed(&signature)
        };

        // Submit the raw transaction
        let pending = provider.send_raw_transaction(signed_tx).await?;

        Ok(pending.tx_hash().to_alloy())
    }

    async fn estimate_gas<T>(&self, tx: &mut TypedTransaction, provider: &Provider<T>) -> Result<()>
    where
        T: JsonRpcClient,
    {
        // if already set, some RPC endpoints might simply return the gas value that is already
        // set in the request and omit the estimate altogether, so we remove it here
        let _ = tx.gas_mut().take();

        tx.set_gas(
            provider
                .estimate_gas(tx, None)
                .await
                .wrap_err_with(|| format!("Failed to estimate gas for tx: {:?}", tx.sighash()))? *
                self.gas_estimate_multiplier /
                100,
        );
        Ok(())
    }
}

/// How to send a single transaction
#[derive(Clone)]
enum SendTransactionKind<'a> {
    Unlocked(Address),
    Raw(&'a WalletSigner),
}

/// Represents how to send _all_ transactions
enum SendTransactionsKind<'a> {
    /// Send via `eth_sendTransaction` and rely on the  `from` address being unlocked.
    Unlocked(HashSet<Address>),
    /// Send a signed transaction via `eth_sendRawTransaction`
    Raw(&'a HashMap<Address, WalletSigner>),
}

impl SendTransactionsKind<'_> {
    /// Returns the [`SendTransactionKind`] for the given address
    ///
    /// Returns an error if no matching signer is found or the address is not unlocked
    fn for_sender(&self, addr: &Address) -> Result<SendTransactionKind<'_>> {
        match self {
            SendTransactionsKind::Unlocked(unlocked) => {
                if !unlocked.contains(addr) {
                    bail!("Sender address {:?} is not unlocked", addr)
                }
                Ok(SendTransactionKind::Unlocked(*addr))
            }
            SendTransactionsKind::Raw(wallets) => {
                if let Some(wallet) = wallets.get(addr) {
                    Ok(SendTransactionKind::Raw(wallet))
                } else {
                    bail!("No matching signer for {:?} found", addr)
                }
            }
        }
    }

    /// How many signers are set
    fn signers_count(&self) -> usize {
        match self {
            SendTransactionsKind::Unlocked(addr) => addr.len(),
            SendTransactionsKind::Raw(signers) => signers.len(),
        }
    }
}
