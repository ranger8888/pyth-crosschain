#![deny(warnings)]
pub mod cli;
use std::str::FromStr;

use anchor_client::{
    anchor_lang::{
        AccountDeserialize,
        AnchorDeserialize,
        AnchorSerialize,
        InstructionData as AnchorInstructionData,
        Owner,
        ToAccountMetas,
    },
    solana_sdk::bpf_loader_upgradeable,
};
use clap::Parser;
use cli::{
    Action,
    Cli,
};

use anyhow::Result;
use remote_executor::{
    accounts::ExecutePostedVaa,
    state::governance_payload::InstructionData,
    EXECUTOR_KEY_SEED,
    ID,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{
        AccountMeta,
        Instruction,
    },
    pubkey::Pubkey,
    signature::{
        read_keypair_file,
        Keypair,
    },
    signer::Signer,
    system_instruction,
    system_instruction::transfer,
    transaction::Transaction,
};
use wormhole_solana::{
    instructions::{
        post_message,
        post_vaa,
        verify_signatures_txs,
        PostVAAData,
    },
    Account,
    Config,
    FeeCollector,
    GuardianSet,
    VAA as PostedVAA,
};

use remote_executor::state::{
    governance_payload::{
        ExecutorPayload,
        GovernanceHeader,
    },
    posted_vaa::AnchorVaa,
};
use wormhole::VAA;

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.action {
        Action::PostAndExecute { vaa, keypair } => {
            let payer =
                read_keypair_file(&*shellexpand::tilde(&keypair)).expect("Keypair not found");
            let rpc_client =
                RpcClient::new_with_commitment("https://pythnet.rpcpool.com/", cli.commitment);

            let vaa_bytes: Vec<u8> = base64::decode(vaa)?;
            let wormhole = AnchorVaa::owner();

            let wormhole_config = Config::key(&wormhole, ());
            let wormhole_config_data =
                Config::try_from_slice(&rpc_client.get_account_data(&wormhole_config)?)?;

            let guardian_set = GuardianSet::key(&wormhole, wormhole_config_data.guardian_set_index);
            let guardian_set_data =
                GuardianSet::try_from_slice(&rpc_client.get_account_data(&guardian_set)?)?;

            let signature_set_keypair = Keypair::new();

            let vaa = VAA::from_bytes(vaa_bytes.clone())?;

            // RENT HACK STARTS HERE
            let signature_set_size = 4 + 19 + 32 + 4;
            let posted_vaa_size = 3 + 1 + 1 + 4 + 32 + 4 + 4 + 8 + 2 + 32 + 4 + vaa.payload.len();
            let posted_vaa_key = PostedVAA::key(&wormhole, vaa.digest().unwrap().hash);

            process_transaction(
                &rpc_client,
                vec![
                    transfer(
                        &payer.pubkey(),
                        &signature_set_keypair.pubkey(),
                        rpc_client.get_minimum_balance_for_rent_exemption(signature_set_size)?,
                    ),
                    transfer(
                        &payer.pubkey(),
                        &posted_vaa_key,
                        rpc_client.get_minimum_balance_for_rent_exemption(posted_vaa_size)?,
                    ),
                ],
                &vec![&payer],
            )?;

            // RENT HACK ENDS HERE

            // First verify VAA
            let verify_txs = verify_signatures_txs(
                vaa_bytes.as_slice(),
                guardian_set_data,
                wormhole,
                payer.pubkey(),
                wormhole_config_data.guardian_set_index,
                signature_set_keypair.pubkey(),
            )?;

            for tx in verify_txs {
                process_transaction(&rpc_client, tx, &vec![&payer, &signature_set_keypair])?;
            }

            // Post VAA
            let post_vaa_data = PostVAAData {
                version: vaa.version,
                guardian_set_index: vaa.guardian_set_index,
                timestamp: vaa.timestamp,
                nonce: vaa.nonce,
                emitter_chain: vaa.emitter_chain.into(),
                emitter_address: vaa.emitter_address,
                sequence: vaa.sequence,
                consistency_level: vaa.consistency_level,
                payload: vaa.payload,
            };

            process_transaction(
                &rpc_client,
                vec![post_vaa(
                    wormhole,
                    payer.pubkey(),
                    signature_set_keypair.pubkey(),
                    post_vaa_data,
                )?],
                &vec![&payer],
            )?;

            // Now execute
            process_transaction(
                &rpc_client,
                vec![get_execute_instruction(
                    &rpc_client,
                    &posted_vaa_key,
                    &payer.pubkey(),
                )?],
                &vec![&payer],
            )?;

            Ok(())
        }

        Action::SendTestVAA { keypair } => {
            let payer =
                read_keypair_file(&*shellexpand::tilde(&keypair)).expect("Keypair not found");
            let rpc_client = RpcClient::new_with_commitment(
                "https://api.mainnet-beta.solana.com",
                cli.commitment,
            );

            let message_keypair = Keypair::new();
            let wormhole = Pubkey::from_str("worm2ZoG2kUd4vFXhvjh93UUH596ayRfgQ2MgjNMTth")?;

            let fee_collector = FeeCollector::key(&wormhole, ());
            let wormhole_config = Config::key(&wormhole, ());

            let wormhole_config_data =
                Config::try_from_slice(&rpc_client.get_account_data(&wormhole_config)?)?;

            let payload = ExecutorPayload {
                header: GovernanceHeader::executor_governance_header(),
                instructions: vec![],
            }
            .try_to_vec()?;

            let transfer_instruction = system_instruction::transfer(
                &payer.pubkey(),
                &fee_collector,
                wormhole_config_data.params.fee,
            );
            let post_vaa_instruction = post_message(
                wormhole,
                payer.pubkey(),
                payer.pubkey(),
                message_keypair.pubkey(),
                0,
                payload.as_slice(),
                0,
            )?;

            process_transaction(
                &rpc_client,
                vec![transfer_instruction, post_vaa_instruction],
                &vec![&payer, &message_keypair],
            )
        }
        Action::GetTestPayload {} => {
            let payload = ExecutorPayload {
                header: GovernanceHeader::executor_governance_header(),
                instructions: vec![],
            }
            .try_to_vec()?;
            println!("Test payload : {:?}", hex::encode(payload));
            Ok(())
        }
        Action::MapKey { pubkey } => {
            let executor_key = Pubkey::find_program_address(
                &[EXECUTOR_KEY_SEED.as_bytes(), &pubkey.to_bytes()],
                &ID,
            )
            .0;
            println!("{:?} maps to {:?}", pubkey, executor_key);
            Ok(())
        }

        Action::GetSetUpgradeAuthorityPayload {
            current,
            new,
            program_id,
        } => {
            let mut instruction =
                bpf_loader_upgradeable::set_upgrade_authority(&program_id, &current, Some(&new));
            instruction.accounts[2].is_signer = true; // Require signature of new authority for safety
            println!("New authority : {:}", instruction.accounts[2].pubkey);
            let payload = ExecutorPayload {
                header: GovernanceHeader::executor_governance_header(),
                instructions: vec![InstructionData::from(&instruction)],
            }
            .try_to_vec()?;
            println!("Set upgrade authority payload : {:?}", hex::encode(payload));
            Ok(())
        }

        Action::GetUpgradeProgramPayload {
            program_id,
            authority,
            new_buffer,
            spill,
        } => {
            let instruction =
                bpf_loader_upgradeable::upgrade(&program_id, &new_buffer, &authority, &spill);
            println!("New buffer : {:}", instruction.accounts[2].pubkey);
            println!(
                "Extra PGAS will be sent to : {:}",
                instruction.accounts[3].pubkey
            );
            let payload = ExecutorPayload {
                header: GovernanceHeader::executor_governance_header(),
                instructions: vec![InstructionData::from(&instruction)],
            }
            .try_to_vec()?;
            println!("Upgrade program payload : {:?}", hex::encode(payload));
            Ok(())
        }
    }
}

pub fn process_transaction(
    rpc_client: &RpcClient,
    instructions: Vec<Instruction>,
    signers: &Vec<&Keypair>,
) -> Result<()> {
    let mut transaction =
        Transaction::new_with_payer(instructions.as_slice(), Some(&signers[0].pubkey()));
    transaction.sign(signers, rpc_client.get_latest_blockhash()?);
    let transaction_signature =
        rpc_client.send_and_confirm_transaction_with_spinner(&transaction)?;
    println!("Transaction successful : {:?}", transaction_signature);
    Ok(())
}

pub fn get_execute_instruction(
    rpc_client: &RpcClient,
    posted_vaa_key: &Pubkey,
    payer_pubkey: &Pubkey,
) -> Result<Instruction> {
    let anchor_vaa =
        AnchorVaa::try_deserialize(&mut rpc_client.get_account_data(posted_vaa_key)?.as_slice())?;
    let emitter = Pubkey::new(&anchor_vaa.emitter_address);

    // First accounts from the anchor context
    let mut account_metas = ExecutePostedVaa::populate(&ID, payer_pubkey, &emitter, posted_vaa_key)
        .to_account_metas(None);

    // Look at the payload
    let executor_payload: ExecutorPayload =
        AnchorDeserialize::try_from_slice(anchor_vaa.payload.as_slice()).unwrap();

    // We need to add `executor_key` to the list of accounts
    let executor_key = Pubkey::find_program_address(
        &[EXECUTOR_KEY_SEED.as_bytes(), &anchor_vaa.emitter_address],
        &ID,
    )
    .0;

    account_metas.push(AccountMeta {
        pubkey: executor_key,
        is_signer: false,
        is_writable: true,
    });

    // Add the rest of `remaining_accounts` from the payload
    for instruction in executor_payload.instructions {
        // Push program_id
        account_metas.push(AccountMeta {
            pubkey: instruction.program_id,
            is_signer: false,
            is_writable: false,
        });
        // Push other accounts
        for account_meta in Instruction::from(&instruction).accounts {
            if account_meta.pubkey != executor_key {
                account_metas.push(account_meta.clone());
            }
        }
    }

    Ok(Instruction {
        program_id: ID,
        accounts: account_metas,
        data: remote_executor::instruction::ExecutePostedVaa.data(),
    })
}
