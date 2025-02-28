use core::num::{NonZeroU128, NonZeroU16};
use std::rc::Rc;
use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;

use anchor_client::anchor_lang::system_program;
use anchor_client::solana_client::rpc_client::RpcClient;
use anchor_client::solana_client::rpc_config::RpcSendTransactionConfig;
use anchor_client::solana_sdk::commitment_config::CommitmentConfig;
use anchor_client::solana_sdk::compute_budget::ComputeBudgetInstruction;
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::signature::{
    read_keypair_file, Keypair, Signature, Signer,
};
use anchor_client::solana_sdk::transaction::Transaction;
use anchor_client::{Client, Cluster};
use anchor_lang::solana_program::system_instruction::create_account;
use anchor_spl::associated_token::get_associated_token_address;
use anyhow::Result;
use ibc::apps::transfer::types::msgs::transfer::MsgTransfer;
use spl_token::instruction::initialize_mint2;

use crate::ibc::ClientStateCommon;
use crate::{
    accounts, chain, ibc, instruction, ix_data_account, CryptoHash,
    MINT_ESCROW_SEED,
};

const IBC_TRIE_PREFIX: &[u8] = b"ibc/";
pub const STAKING_PROGRAM_ID: &str =
    "8n3FHwYxFgQCQc2FNFkwDUf9mcqupxXcCvgfHbApMLv3";
pub const WRITE_ACCOUNT_SEED: &[u8] = b"write";
// const BASE_DENOM: &str = "PICA";

const TRANSFER_AMOUNT: u64 = 1000000;

fn airdrop(client: &RpcClient, account: Pubkey, lamports: u64) -> Signature {
    let balance_before = client.get_balance(&account).unwrap();
    println!("This is balance before {}", balance_before);
    let airdrop_signature = client.request_airdrop(&account, lamports).unwrap();
    sleep(Duration::from_secs(2));
    println!("This is airdrop signature {}", airdrop_signature);

    let balance_after = client.get_balance(&account).unwrap();
    println!("This is balance after {}", balance_after);
    assert_eq!(balance_before + lamports, balance_after);
    airdrop_signature
}

fn create_mock_client_and_cs_state(
) -> (ibc::mock::MockClientState, ibc::mock::MockConsensusState) {
    let mock_header = ibc::mock::MockHeader {
        height: ibc::Height::min(0),
        timestamp: ibc::Timestamp::from_nanoseconds(1).unwrap(),
    };
    let mock_client_state = ibc::mock::MockClientState::new(mock_header);
    let mock_cs_state = ibc::mock::MockConsensusState::new(mock_header);
    (mock_client_state, mock_cs_state)
}

macro_rules! make_message {
    ($msg:expr, $($variant:path),+ $(,)?) => {{
        let message = $msg;
        $( let message = $variant(message); )*
        message
    }}
}

#[test]
#[ignore = "Requires local validator to run"]
fn anchor_test_deliver() -> Result<()> {
    let authority = Rc::new(read_keypair_file("../../keypair.json").unwrap());
    println!("This is pubkey {}", authority.pubkey().to_string());
    let lamports = 2_000_000_000;

    let client = Client::new_with_options(
        Cluster::Localnet,
        authority.clone(),
        CommitmentConfig::processed(),
    );
    let program = client.program(crate::ID).unwrap();
    let write_account_program_id =
        read_keypair_file("../../../../target/deploy/write-keypair.json")
            .unwrap()
            .pubkey();

    let sol_rpc_client = program.rpc();
    let _airdrop_signature =
        airdrop(&sol_rpc_client, authority.pubkey(), lamports);

    // Build, sign, and send program instruction
    let storage = Pubkey::find_program_address(
        &[crate::SOLANA_IBC_STORAGE_SEED],
        &crate::ID,
    )
    .0;
    let trie = Pubkey::find_program_address(&[crate::TRIE_SEED], &crate::ID).0;
    let chain =
        Pubkey::find_program_address(&[crate::CHAIN_SEED], &crate::ID).0;

    let mint_keypair = Keypair::new();
    let native_token_mint_key = mint_keypair.pubkey();
    let base_denom = native_token_mint_key.to_string();
    let hashed_denom = CryptoHash::digest(base_denom.as_bytes());

    /*
     * Initialise chain
     */
    println!("\nInitialising");
    let sig = program
        .request()
        .accounts(accounts::Initialise {
            sender: authority.pubkey(),
            storage,
            trie,
            chain,
            system_program: system_program::ID,
        })
        .args(instruction::Initialise {
            config: chain::Config {
                min_validators: NonZeroU16::MIN,
                max_validators: NonZeroU16::MAX,
                min_validator_stake: NonZeroU128::new(1000).unwrap(),
                min_total_stake: NonZeroU128::new(1000).unwrap(),
                min_quorum_stake: NonZeroU128::new(1000).unwrap(),
                min_block_length: 5.into(),
                min_epoch_length: 200_000.into(),
            },
            staking_program_id: Pubkey::from_str(STAKING_PROGRAM_ID).unwrap(),
            genesis_epoch: chain::Epoch::new(
                vec![chain::Validator::new(
                    authority.pubkey().into(),
                    NonZeroU128::new(2000).unwrap(),
                )],
                NonZeroU128::new(1000).unwrap(),
            )
            .unwrap(),
        })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    let chain_account: chain::ChainData = program.account(chain).unwrap();

    let genesis_hash = chain_account.genesis().unwrap();
    println!("This is genesis hash {}", genesis_hash.to_string());

    /*
     * Create New Mock Client
     */
    println!("\nCreating Mock Client");
    let (mock_client_state, mock_cs_state) = create_mock_client_and_cs_state();
    let message = make_message!(
        ibc::MsgCreateClient::new(
            ibc::Any::from(mock_client_state),
            ibc::Any::from(mock_cs_state),
            ibc::Signer::from(authority.pubkey().to_string()),
        ),
        ibc::ClientMsg::CreateClient,
        ibc::MsgEnvelope::Client,
    );

    println!(
        "\nSplitting the message into chunks and sending it to write-account \
         program"
    );
    let mut instruction_data =
        anchor_lang::InstructionData::data(&instruction::Deliver { message });
    let instruction_len = instruction_data.len() as u32;
    instruction_data.splice(..0, instruction_len.to_le_bytes());

    let blockhash = sol_rpc_client.get_latest_blockhash().unwrap();

    let (mut chunks, chunk_account, _) = write::instruction::WriteIter::new(
        &write_account_program_id,
        authority.pubkey(),
        WRITE_ACCOUNT_SEED,
        instruction_data,
    )
    .unwrap();
    // Note: We’re using small chunks size on purpose to test the behaviour of
    // the write account program.
    chunks.chunk_size = core::num::NonZeroU16::new(50).unwrap();
    for instruction in &mut chunks {
        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&authority.pubkey()),
            &[&*authority],
            blockhash,
        );
        let sig = sol_rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .unwrap();
        println!("  Signature {sig}");
    }
    let (write_account, write_account_bump) = chunks.into_account();

    println!("\nCreating Mock Client");
    let sig = program
        .request()
        .accounts(ix_data_account::Accounts::new(
            accounts::Deliver {
                sender: authority.pubkey(),
                receiver: None,
                storage,
                trie,
                chain,
                system_program: system_program::ID,
                mint_authority: None,
                token_mint: None,
                escrow_account: None,
                receiver_token_account: None,
                associated_token_program: None,
                token_program: None,
            },
            chunk_account,
        ))
        .args(ix_data_account::Instruction)
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    /*
     * Create New Mock Connection Open Init
     */
    println!("\nIssuing Connection Open Init");
    let client_id = mock_client_state.client_type().build_client_id(0);
    let counter_party_client_id =
        mock_client_state.client_type().build_client_id(1);

    let commitment_prefix: ibc::CommitmentPrefix =
        IBC_TRIE_PREFIX.to_vec().try_into().unwrap();

    let message = make_message!(
        ibc::MsgConnectionOpenInit {
            client_id_on_a: mock_client_state.client_type().build_client_id(0),
            version: Some(Default::default()),
            counterparty: ibc::conn::Counterparty::new(
                counter_party_client_id.clone(),
                None,
                commitment_prefix.clone(),
            ),
            delay_period: Duration::from_secs(5),
            signer: ibc::Signer::from(authority.pubkey().to_string()),
        },
        ibc::ConnectionMsg::OpenInit,
        ibc::MsgEnvelope::Connection,
    );

    let sig = program
        .request()
        .accounts(accounts::Deliver {
            sender: authority.pubkey(),
            receiver: None,
            storage,
            trie,
            chain,
            system_program: system_program::ID,
            mint_authority: None,
            token_mint: None,
            escrow_account: None,
            receiver_token_account: None,
            associated_token_program: None,
            token_program: None,
        })
        .args(instruction::Deliver { message })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    let port_id = ibc::PortId::transfer();
    let channel_id_on_a = ibc::ChannelId::new(0);
    let channel_id_on_b = ibc::ChannelId::new(1);

    let seeds =
        [port_id.as_bytes(), channel_id_on_a.as_bytes(), hashed_denom.as_ref()];
    let (escrow_account_key, _bump) =
        Pubkey::find_program_address(&seeds, &crate::ID);
    let (token_mint_key, _bump) =
        Pubkey::find_program_address(&[hashed_denom.as_ref()], &crate::ID);
    let (mint_authority_key, _bump) =
        Pubkey::find_program_address(&[MINT_ESCROW_SEED], &crate::ID);

    /*
     * Setup mock connection and channel
     *
     * Steps before we proceed
     *  - Create PDAs for the above keys,
     *  - Get token account for receiver and sender
     */
    println!("\nSetting up mock connection and channel");
    let receiver = Keypair::new();

    let sender_token_address =
        get_associated_token_address(&authority.pubkey(), &token_mint_key);
    let receiver_token_address =
        get_associated_token_address(&receiver.pubkey(), &token_mint_key);

    let sig = program
        .request()
        .instruction(ComputeBudgetInstruction::set_compute_unit_limit(
            1_000_000u32,
        ))
        .accounts(accounts::MockDeliver {
            sender: authority.pubkey(),
            storage,
            trie,
            chain,
            system_program: system_program::ID,
        })
        .args(instruction::MockDeliver {
            port_id: port_id.clone(),
            commitment_prefix,
            client_id: client_id.clone(),
            counterparty_client_id: counter_party_client_id,
        })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    // Make sure all the accounts needed for transfer are ready ( mint, escrow etc.)
    // Pass the instruction for transfer

    /*
     * Setup deliver escrow.
     */
    let sig = program
        .request()
        .instruction(ComputeBudgetInstruction::set_compute_unit_limit(
            1_000_000u32,
        ))
        .accounts(accounts::InitMint {
            sender: authority.pubkey(),
            mint_authority: mint_authority_key,
            // escrow_account: escrow_account_key,
            token_mint: token_mint_key,
            system_program: system_program::ID,
            associated_token_program: anchor_spl::associated_token::ID,
            token_program: anchor_spl::token::ID,
        })
        .args(instruction::InitMint {
            port_id: port_id.clone(),
            channel_id_on_b: channel_id_on_a.clone(),
            hashed_base_denom: hashed_denom.clone(),
        })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    let mint_info = sol_rpc_client.get_token_supply(&token_mint_key).unwrap();

    println!("  This is the mint information {:?}", mint_info);

    /*
     * Creating Token Mint
     */
    println!("\nCreating a token mint");

    let create_account_ix = create_account(
        &authority.pubkey(),
        &native_token_mint_key,
        sol_rpc_client.get_minimum_balance_for_rent_exemption(82).unwrap(),
        82,
        &anchor_spl::token::ID,
    );

    let create_mint_ix = initialize_mint2(
        &anchor_spl::token::ID,
        &native_token_mint_key,
        &authority.pubkey(),
        Some(&authority.pubkey()),
        6,
    )
    .expect("invalid mint instruction");

    let create_token_acc_ix = spl_associated_token_account::instruction::create_associated_token_account(&authority.pubkey(), &authority.pubkey(), &native_token_mint_key, &anchor_spl::token::ID);
    let associated_token_addr = get_associated_token_address(
        &authority.pubkey(),
        &native_token_mint_key,
    );
    let mint_ix = spl_token::instruction::mint_to(
        &anchor_spl::token::ID,
        &native_token_mint_key,
        &associated_token_addr,
        &authority.pubkey(),
        &[&authority.pubkey()],
        1000000000,
    )
    .unwrap();

    let tx = program
        .request()
        .instruction(create_account_ix)
        .instruction(create_mint_ix)
        .instruction(create_token_acc_ix)
        .instruction(mint_ix)
        .payer(authority.clone())
        .signer(&*authority)
        .signer(&mint_keypair)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;

    println!("  Signature: {}", tx);

    /*
     * Sending transfer on source chain
     */
    println!("\nSend Transfer On Source Chain");

    let msg_transfer = construct_transfer_packet_from_denom(
        &base_denom,
        port_id.clone(),
        channel_id_on_b.clone(),
        channel_id_on_a.clone(),
        associated_token_addr,
        receiver_token_address,
    );

    let account_balance_before = sol_rpc_client
        .get_token_account_balance(&associated_token_addr)
        .unwrap();

    let sig = program
        .request()
        .instruction(ComputeBudgetInstruction::set_compute_unit_limit(
            1_000_000u32,
        ))
        .accounts(accounts::SendTransfer {
            sender: authority.pubkey(),
            receiver: Some(receiver.pubkey()),
            storage,
            trie,
            chain,
            system_program: system_program::ID,
            mint_authority: Some(mint_authority_key),
            token_mint: Some(native_token_mint_key),
            escrow_account: Some(escrow_account_key),
            receiver_token_account: Some(associated_token_addr),
            associated_token_program: Some(anchor_spl::associated_token::ID),
            token_program: Some(anchor_spl::token::ID),
        })
        .args(instruction::SendTransfer {
            port_id: port_id.clone(),
            channel_id: channel_id_on_a.clone(),
            hashed_base_denom: hashed_denom.clone(),
            msg: msg_transfer,
        })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    let account_balance_after = sol_rpc_client
        .get_token_account_balance(&associated_token_addr)
        .unwrap();

    assert_eq!(
        ((account_balance_before.ui_amount.unwrap() -
            account_balance_after.ui_amount.unwrap()) *
            10_f64.powf(mint_info.decimals.into()))
        .round() as u64,
        TRANSFER_AMOUNT
    );

    /*
     * On Destination chain
     */
    println!("\nRecving on destination chain");
    let account_balance_before = sol_rpc_client
        .get_token_account_balance(&receiver_token_address)
        .map_or(0f64, |balance| balance.ui_amount.unwrap());

    let packet = construct_packet_from_denom(
        &base_denom,
        port_id.clone(),
        channel_id_on_b.clone(),
        channel_id_on_a.clone(),
        channel_id_on_b.clone(),
        2,
        sender_token_address,
        receiver_token_address,
        String::from("Tx from destination chain"),
    );
    let proof_height_on_a = mock_client_state.header.height;

    let message = make_message!(
        ibc::MsgRecvPacket {
            packet: packet.clone(),
            proof_commitment_on_a: ibc::CommitmentProofBytes::try_from(
                packet.data
            )
            .unwrap(),
            proof_height_on_a,
            signer: ibc::Signer::from(authority.pubkey().to_string())
        },
        ibc::PacketMsg::Recv,
        ibc::MsgEnvelope::Packet,
    );

    let sig = program
        .request()
        .instruction(ComputeBudgetInstruction::set_compute_unit_limit(
            1_000_000u32,
        ))
        .accounts(accounts::Deliver {
            sender: authority.pubkey(),
            receiver: Some(receiver.pubkey()),
            storage,
            trie,
            chain,
            system_program: system_program::ID,
            mint_authority: Some(mint_authority_key),
            token_mint: Some(token_mint_key),
            escrow_account: None,
            receiver_token_account: Some(receiver_token_address),
            associated_token_program: Some(anchor_spl::associated_token::ID),
            token_program: Some(anchor_spl::token::ID),
        })
        .args(instruction::Deliver { message })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    let account_balance_after = sol_rpc_client
        .get_token_account_balance(&receiver_token_address)
        .unwrap();
    assert_eq!(
        ((account_balance_after.ui_amount.unwrap() - account_balance_before) *
            10_f64.powf(mint_info.decimals.into()))
        .round() as u64,
        TRANSFER_AMOUNT
    );

    /*
     * Sending transfer on destination chain
     */
    println!("\nSend Transfer On Destination Chain");

    let msg_transfer = construct_transfer_packet_from_denom(
        &base_denom,
        port_id.clone(),
        channel_id_on_a.clone(),
        channel_id_on_a.clone(),
        associated_token_addr,
        receiver_token_address,
    );

    let account_balance_before = sol_rpc_client
        .get_token_account_balance(&associated_token_addr)
        .unwrap();

    let sig = program
        .request()
        .instruction(ComputeBudgetInstruction::set_compute_unit_limit(
            1_000_000u32,
        ))
        .accounts(accounts::SendTransfer {
            sender: authority.pubkey(),
            receiver: Some(receiver.pubkey()),
            storage,
            trie,
            chain,
            system_program: system_program::ID,
            mint_authority: Some(mint_authority_key),
            token_mint: Some(native_token_mint_key),
            escrow_account: Some(escrow_account_key),
            receiver_token_account: Some(associated_token_addr),
            associated_token_program: Some(anchor_spl::associated_token::ID),
            token_program: Some(anchor_spl::token::ID),
        })
        .args(instruction::SendTransfer {
            port_id: port_id.clone(),
            channel_id: channel_id_on_a.clone(),
            hashed_base_denom: hashed_denom,
            msg: msg_transfer,
        })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    let account_balance_after = sol_rpc_client
        .get_token_account_balance(&associated_token_addr)
        .unwrap();

    assert_eq!(
        ((account_balance_before.ui_amount.unwrap() -
            account_balance_after.ui_amount.unwrap()) *
            10_f64.powf(mint_info.decimals.into()))
        .round() as u64,
        TRANSFER_AMOUNT
    );

    /*
     * On Source chain
     */
    println!("\nRecving on source chain");

    let receiver_native_token_address = get_associated_token_address(
        &receiver.pubkey(),
        &native_token_mint_key,
    );

    let packet = construct_packet_from_denom(
        &base_denom,
        port_id.clone(),
        channel_id_on_b.clone(),
        channel_id_on_b.clone(),
        channel_id_on_a.clone(),
        3,
        sender_token_address,
        receiver_native_token_address,
        String::from("Tx from Source chain"),
    );

    let proof_height_on_a = mock_client_state.header.height;

    let message = make_message!(
        ibc::MsgRecvPacket {
            packet: packet.clone(),
            proof_commitment_on_a: ibc::CommitmentProofBytes::try_from(
                packet.data
            )
            .unwrap(),
            proof_height_on_a,
            signer: ibc::Signer::from(authority.pubkey().to_string())
        },
        ibc::PacketMsg::Recv,
        ibc::MsgEnvelope::Packet,
    );

    // println!("  This is trie {:?}", trie);
    // println!("  This is storage {:?}", storage);

    let escrow_account_balance_before =
        sol_rpc_client.get_token_account_balance(&escrow_account_key).unwrap();
    let receiver_account_balance_before = sol_rpc_client
        .get_token_account_balance(&receiver_native_token_address)
        .map_or(0f64, |balance| balance.ui_amount.unwrap());

    let sig = program
        .request()
        .instruction(ComputeBudgetInstruction::set_compute_unit_limit(
            1_000_000u32,
        ))
        .accounts(accounts::Deliver {
            sender: authority.pubkey(),
            receiver: Some(receiver.pubkey()),
            storage,
            trie,
            chain,
            system_program: system_program::ID,
            mint_authority: Some(mint_authority_key),
            token_mint: Some(native_token_mint_key),
            escrow_account: Some(escrow_account_key),
            receiver_token_account: Some(receiver_native_token_address),
            associated_token_program: Some(anchor_spl::associated_token::ID),
            token_program: Some(anchor_spl::token::ID),
        })
        .args(instruction::Deliver { message })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");

    let escrow_account_balance_after =
        sol_rpc_client.get_token_account_balance(&escrow_account_key).unwrap();
    let receiver_account_balance_after = sol_rpc_client
        .get_token_account_balance(&receiver_native_token_address)
        .unwrap();
    assert_eq!(
        ((escrow_account_balance_before.ui_amount.unwrap() -
            escrow_account_balance_after.ui_amount.unwrap()) *
            10_f64.powf(mint_info.decimals.into()))
        .round() as u64,
        TRANSFER_AMOUNT
    );
    assert_eq!(
        ((receiver_account_balance_after.ui_amount.unwrap() -
            receiver_account_balance_before) *
            10_f64.powf(mint_info.decimals.into()))
        .round() as u64,
        TRANSFER_AMOUNT
    );

    /*
     * Send Packets
     */
    println!("\nSend packet");
    let packet = construct_packet_from_denom(
        &base_denom,
        port_id.clone(),
        channel_id_on_a.clone(),
        channel_id_on_a.clone(),
        channel_id_on_b.clone(),
        1,
        sender_token_address,
        receiver_token_address,
        String::from("Just a packet"),
    );

    let sig = program
        .request()
        .accounts(accounts::SendPacket {
            sender: authority.pubkey(),
            storage,
            trie,
            chain,
            system_program: system_program::ID,
        })
        .args(instruction::SendPacket {
            port_id: port_id.clone(),
            channel_id: channel_id_on_a.clone(),
            data: packet.data,
            timeout_height: packet.timeout_height_on_b,
            timeout_timestamp: packet.timeout_timestamp_on_b,
        })
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature: {sig}");


    /*
     * Free Write account
     */
    println!("\nFreeing Write account");
    let sig = program
        .request()
        .instruction(write::instruction::free(
            write_account_program_id,
            authority.pubkey(),
            Some(write_account),
            WRITE_ACCOUNT_SEED,
            write_account_bump,
        )?)
        .payer(authority.clone())
        .signer(&*authority)
        .send_with_spinner_and_config(RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        })?;
    println!("  Signature {sig}");

    Ok(())
}

fn construct_packet_from_denom(
    base_denom: &str,
    port_id: ibc::PortId,
    // Channel id used to define if its source chain or destination chain (in
    // denom).
    denom_channel_id: ibc::ChannelId,
    channel_id_on_a: ibc::ChannelId,
    channel_id_on_b: ibc::ChannelId,
    sequence: u64,
    sender_token_address: Pubkey,
    receiver_token_address: Pubkey,
    memo: String,
) -> ibc::Packet {
    let denom = format!("{port_id}/{denom_channel_id}/{base_denom}");
    let denom =
        ibc::apps::transfer::types::PrefixedDenom::from_str(&denom).unwrap();
    let token = ibc::apps::transfer::types::Coin {
        denom,
        amount: TRANSFER_AMOUNT.into(),
    };

    let packet_data = ibc::apps::transfer::types::packet::PacketData {
        token: token.into(),
        sender: ibc::Signer::from(sender_token_address.to_string()), // Should be a token account
        receiver: ibc::Signer::from(receiver_token_address.to_string()), // Should be a token account
        memo: memo.into(),
    };

    let serialized_data = serde_json::to_vec(&packet_data).unwrap();

    let packet = ibc::Packet {
        seq_on_a: sequence.into(),
        port_id_on_a: port_id.clone(),
        chan_id_on_a: channel_id_on_a,
        port_id_on_b: port_id,
        chan_id_on_b: channel_id_on_b,
        data: serialized_data.clone(),
        timeout_height_on_b: ibc::TimeoutHeight::Never,
        timeout_timestamp_on_b: ibc::Timestamp::none(),
    };

    packet
}

fn construct_transfer_packet_from_denom(
    base_denom: &str,
    port_id: ibc::PortId,
    // Channel id used to define if its source chain or destination chain (in
    // denom).
    denom_channel_id: ibc::ChannelId,
    channel_id_on_a: ibc::ChannelId,
    sender_address: Pubkey,
    receiver_address: Pubkey,
) -> MsgTransfer {
    let denom = format!("{port_id}/{denom_channel_id}/{base_denom}");
    let denom =
        ibc::apps::transfer::types::PrefixedDenom::from_str(&denom).unwrap();
    let token = ibc::apps::transfer::types::Coin {
        denom,
        amount: TRANSFER_AMOUNT.into(),
    };

    let packet_data = ibc::apps::transfer::types::packet::PacketData {
        token: token.into(),
        sender: ibc::Signer::from(sender_address.to_string()), // Should be a token account
        receiver: ibc::Signer::from(receiver_address.to_string()), // Should be a token account
        memo: String::from("Sending a transfer").into(),
    };

    MsgTransfer {
        port_id_on_a: port_id.clone(),
        chan_id_on_a: channel_id_on_a.clone(),
        packet_data,
        timeout_height_on_b: ibc::TimeoutHeight::Never,
        timeout_timestamp_on_b: ibc::Timestamp::none(),
    }
}
