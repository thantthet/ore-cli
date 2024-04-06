use std::{
    io::{stdout, Write}, sync::Arc, time::Duration
};

use rand::{thread_rng, Rng};
use solana_client::{
    client_error::{ClientError, ClientErrorKind, Result as ClientResult},
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig},
};
use solana_program::instruction::Instruction;
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    compute_budget::ComputeBudgetInstruction,
    signature::{read_keypair_file, Signature, Signer},
    transaction::{Transaction, VersionedTransaction},
    system_instruction::transfer,
};
use solana_transaction_status::{TransactionConfirmationStatus, UiTransactionEncoding};

use crate::Miner;

use jito_searcher_client::{
    get_searcher_client, send_bundle_no_wait
};

// const RPC_RETRIES: usize = 0;
const SIMULATION_RETRIES: usize = 4;
const GATEWAY_RETRIES: usize = 4;
const CONFIRM_RETRIES: usize = 4;

// list of jito tip accounts
const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

impl Miner {
    pub async fn send_and_confirm(
        &self,
        ixs: &[Instruction],
        dynamic_cus: bool,
        skip_confirm: bool,
    ) -> ClientResult<Signature> {
        let mut rng = thread_rng();
        let mut stdout = stdout();
        let signer = self.signer();
        let client =
            RpcClient::new_with_commitment(self.cluster.clone(), CommitmentConfig::confirmed());

        let keypair = Arc::new(read_keypair_file(&self.block_engine_auth_keypair).expect("reads keypair at path"));
        let mut searcher_client = get_searcher_client(
            &self.block_engine_url,
            &keypair,
        )
        .await
        .expect("connects to searcher client");

        // Return error if balance is zero
        let balance = client
            .get_balance_with_commitment(&signer.pubkey(), CommitmentConfig::confirmed())
            .await
            .unwrap();
        if balance.value <= 0 {
            return Err(ClientError {
                request: None,
                kind: ClientErrorKind::Custom("Insufficient SOL balance".into()),
            });
        }

        // Build tx
        let (mut hash, mut slot) = client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await
            .unwrap();
        // let mut send_cfg = RpcSendTransactionConfig {
        //     skip_preflight: true,
        //     preflight_commitment: Some(CommitmentLevel::Confirmed),
        //     encoding: Some(UiTransactionEncoding::Base64),
        //     max_retries: Some(RPC_RETRIES),
        //     min_context_slot: Some(slot),
        // };
        let mut tx = Transaction::new_with_payer(ixs, Some(&signer.pubkey()));

        // Simulate if necessary
        if dynamic_cus {
            let mut sim_attempts = 0;
            'simulate: loop {
                let sim_res = client
                    .simulate_transaction_with_config(
                        &tx,
                        RpcSimulateTransactionConfig {
                            sig_verify: false,
                            replace_recent_blockhash: true,
                            commitment: Some(CommitmentConfig::confirmed()),
                            encoding: Some(UiTransactionEncoding::Base64),
                            accounts: None,
                            min_context_slot: None
                        },
                    )
                    .await;
                match sim_res {
                    Ok(sim_res) => {
                        if let Some(err) = sim_res.value.err {
                            println!("Simulaton error: {:?}", err);
                            sim_attempts += 1;
                            if sim_attempts.gt(&SIMULATION_RETRIES) {
                                return Err(ClientError {
                                    request: None,
                                    kind: ClientErrorKind::Custom("Simulation failed".into()),
                                });
                            }
                        } else if let Some(units_consumed) = sim_res.value.units_consumed {
                            println!("Dynamic CUs: {:?}", units_consumed);
                            let cu_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(
                                units_consumed as u32 + 1000,
                            );
                            let mut final_ixs = vec![];
                            final_ixs.extend_from_slice(&[cu_budget_ix]);
                            final_ixs.extend_from_slice(ixs);
                            tx = Transaction::new_with_payer(&final_ixs, Some(&signer.pubkey()));
                            break 'simulate;
                        }
                    }
                    Err(err) => {
                        println!("Simulaton error: {:?}", err);
                        sim_attempts += 1;
                        if sim_attempts.gt(&SIMULATION_RETRIES) {
                            return Err(ClientError {
                                request: None,
                                kind: ClientErrorKind::Custom("Simulation failed".into()),
                            });
                        }
                    }
                }
            }
        }

        // Submit tx
        tx.sign(&[&signer], hash);
        let mut sigs = vec![];
        let mut attempts = 0;
        loop {
            println!("Attempt: {:?}", attempts);

            let tip_account_str = JITO_TIP_ACCOUNTS[rng.gen_range(0..JITO_TIP_ACCOUNTS.len())];
            let tip_account = tip_account_str.parse().unwrap();
            let tip_vtx = VersionedTransaction::from(Transaction::new_signed_with_payer(
                &[
                    transfer(&signer.pubkey(), &tip_account, self.priority_fee),
                ],
                Some(&signer.pubkey()),
                &[&signer],
                hash,
            ));

            let vtx = VersionedTransaction::from(tx.clone());
            let sig = tx.signatures.get(0).unwrap().clone();

            match send_bundle_no_wait(&[vtx, tip_vtx], &mut searcher_client).await {
                Ok(_res) => {
                    sigs.push(sig);
                    println!("{:?}", sig);

                    // Confirm tx
                    if skip_confirm {
                        return Ok(sig);
                    }
                    for _ in 0..CONFIRM_RETRIES {
                        std::thread::sleep(Duration::from_millis(2000));
                        match client.get_signature_statuses(&sigs).await {
                            Ok(signature_statuses) => {
                                println!("Confirms: {:?}", signature_statuses.value);
                                for signature_status in signature_statuses.value {
                                    if let Some(signature_status) = signature_status.as_ref() {
                                        if signature_status.confirmation_status.is_some() {
                                            let current_commitment = signature_status
                                                .confirmation_status
                                                .as_ref()
                                                .unwrap();
                                            match current_commitment {
                                                TransactionConfirmationStatus::Processed => {}
                                                TransactionConfirmationStatus::Confirmed
                                                | TransactionConfirmationStatus::Finalized => {
                                                    println!("Transaction landed!");
                                                    return Ok(sig);
                                                }
                                            }
                                        } else {
                                            println!("No status");
                                        }
                                    }
                                }
                            }

                            // Handle confirmation errors
                            Err(err) => {
                                println!("Error: {:?}", err);
                            }
                        }
                    }
                    println!("Transaction did not land");
                }

                // Handle submit errors
                Err(err) => {
                    println!("Error {:?}", err);
                }
            }
            stdout.flush().ok();

            // Retry
            std::thread::sleep(Duration::from_millis(2000));
            (hash, slot) = client
                .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
                .await
                .unwrap();
            // send_cfg = RpcSendTransactionConfig {
            //     skip_preflight: true,
            //     preflight_commitment: Some(CommitmentLevel::Confirmed),
            //     encoding: Some(UiTransactionEncoding::Base64),
            //     max_retries: Some(RPC_RETRIES),
            //     min_context_slot: Some(slot),
            // };
            tx.sign(&[&signer], hash);
            attempts += 1;
            if attempts > GATEWAY_RETRIES {
                return Err(ClientError {
                    request: None,
                    kind: ClientErrorKind::Custom("Max retries".into()),
                });
            }
        }
    }
}
