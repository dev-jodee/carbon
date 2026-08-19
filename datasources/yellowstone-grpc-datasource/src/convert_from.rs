//! Conversion from the Yellowstone protobuf wire types into the Solana types
//! the pipeline consumes.
//!
//! `yellowstone-grpc-proto` dropped its `convert` feature in 12.0.0 and moved
//! those helpers into `yellowstone-grpc-geyser`, which is AGPL-3.0 and
//! unpublished, so this crate carries its own conversion.

use {
    solana_account_decoder::parse_token::UiTokenAmount,
    solana_hash::{Hash, HASH_BYTES},
    solana_message::{
        compiled_instruction::CompiledInstruction,
        v0::{LoadedAddresses, Message as MessageV0, MessageAddressTableLookup},
        v1::{Message as MessageV1, TransactionConfig},
        Message as MessageLegacy, MessageHeader, VersionedMessage,
    },
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::TransactionError,
    solana_transaction_status::{
        InnerInstruction, InnerInstructions, Reward, RewardType, TransactionStatusMeta,
        TransactionTokenBalance,
    },
    yellowstone_grpc_proto::prelude as proto,
};

pub type ConvertResult<T> = Result<T, &'static str>;

pub fn create_tx_versioned(tx: proto::Transaction) -> ConvertResult<VersionedTransaction> {
    let mut signatures = Vec::with_capacity(tx.signatures.len());
    for signature in tx.signatures {
        signatures.push(
            Signature::try_from(signature.as_slice()).map_err(|_| "failed to parse Signature")?,
        );
    }

    Ok(VersionedTransaction {
        signatures,
        message: create_message(tx.message.ok_or("failed to get Message")?)?,
    })
}

pub fn create_message(message: proto::Message) -> ConvertResult<VersionedMessage> {
    let header = create_message_header(message.header.ok_or("failed to get MessageHeader")?)?;
    let blockhash = create_hash(&message.recent_blockhash)?;
    let account_keys = create_pubkey_vec(message.account_keys)?;
    let instructions = create_compiled_instructions(message.instructions)?;

    // `versioned` is true for both V0 and V1, so only the presence of `config`
    // distinguishes them. V1 carries its blockhash in `lifetime_specifier` and
    // never has address table lookups.
    if let Some(config) = message.config {
        return Ok(VersionedMessage::V1(MessageV1 {
            header,
            config: TransactionConfig {
                priority_fee: config.priority_fee,
                compute_unit_limit: config.compute_unit_limit,
                loaded_accounts_data_size_limit: config.loaded_accounts_data_size_limit,
                heap_size: config.heap_size,
            },
            lifetime_specifier: blockhash,
            account_keys,
            instructions,
        }));
    }

    if !message.versioned {
        return Ok(VersionedMessage::Legacy(MessageLegacy {
            header,
            account_keys,
            recent_blockhash: blockhash,
            instructions,
        }));
    }

    let mut address_table_lookups = Vec::with_capacity(message.address_table_lookups.len());
    for lookup in message.address_table_lookups {
        address_table_lookups.push(MessageAddressTableLookup {
            account_key: create_pubkey(&lookup.account_key)?,
            writable_indexes: lookup.writable_indexes,
            readonly_indexes: lookup.readonly_indexes,
        });
    }

    Ok(VersionedMessage::V0(MessageV0 {
        header,
        account_keys,
        recent_blockhash: blockhash,
        instructions,
        address_table_lookups,
    }))
}

pub fn create_tx_meta(meta: proto::TransactionStatusMeta) -> ConvertResult<TransactionStatusMeta> {
    let status = match create_tx_error(meta.err.as_ref())? {
        Some(error) => Err(error),
        None => Ok(()),
    };

    Ok(TransactionStatusMeta {
        status,
        fee: meta.fee,
        pre_balances: meta.pre_balances,
        post_balances: meta.post_balances,
        inner_instructions: if meta.inner_instructions_none {
            None
        } else {
            Some(create_inner_instructions_vec(meta.inner_instructions)?)
        },
        log_messages: if meta.log_messages_none {
            None
        } else {
            Some(meta.log_messages)
        },
        pre_token_balances: Some(create_token_balances(meta.pre_token_balances)?),
        post_token_balances: Some(create_token_balances(meta.post_token_balances)?),
        rewards: Some(create_rewards(meta.rewards)?),
        loaded_addresses: LoadedAddresses {
            writable: create_pubkey_vec(meta.loaded_writable_addresses)?,
            readonly: create_pubkey_vec(meta.loaded_readonly_addresses)?,
        },
        return_data: if meta.return_data_none {
            None
        } else {
            meta.return_data
                .map(|data| {
                    Ok::<_, &'static str>(TransactionReturnData {
                        program_id: create_pubkey(&data.program_id)?,
                        data: data.data,
                    })
                })
                .transpose()?
        },
        compute_units_consumed: meta.compute_units_consumed,
        cost_units: meta.cost_units,
    })
}

pub fn create_tx_error(
    err: Option<&proto::TransactionError>,
) -> ConvertResult<Option<TransactionError>> {
    err.map(|err| wincode::deserialize::<TransactionError>(&err.err))
        .transpose()
        .map_err(|_| "failed to decode TransactionError")
}

fn create_message_header(header: proto::MessageHeader) -> ConvertResult<MessageHeader> {
    Ok(MessageHeader {
        num_required_signatures: header
            .num_required_signatures
            .try_into()
            .map_err(|_| "failed to parse num_required_signatures")?,
        num_readonly_signed_accounts: header
            .num_readonly_signed_accounts
            .try_into()
            .map_err(|_| "failed to parse num_readonly_signed_accounts")?,
        num_readonly_unsigned_accounts: header
            .num_readonly_unsigned_accounts
            .try_into()
            .map_err(|_| "failed to parse num_readonly_unsigned_accounts")?,
    })
}

fn create_compiled_instructions(
    instructions: Vec<proto::CompiledInstruction>,
) -> ConvertResult<Vec<CompiledInstruction>> {
    instructions
        .into_iter()
        .map(create_compiled_instruction)
        .collect()
}

fn create_compiled_instruction(
    instruction: proto::CompiledInstruction,
) -> ConvertResult<CompiledInstruction> {
    Ok(CompiledInstruction {
        program_id_index: instruction
            .program_id_index
            .try_into()
            .map_err(|_| "failed to parse program_id_index")?,
        accounts: instruction.accounts,
        data: instruction.data,
    })
}

fn create_inner_instructions_vec(
    inner_instructions: Vec<proto::InnerInstructions>,
) -> ConvertResult<Vec<InnerInstructions>> {
    inner_instructions
        .into_iter()
        .map(|inner| {
            Ok(InnerInstructions {
                index: inner
                    .index
                    .try_into()
                    .map_err(|_| "failed to parse InnerInstructions index")?,
                instructions: inner
                    .instructions
                    .into_iter()
                    .map(|instruction| {
                        Ok(InnerInstruction {
                            instruction: CompiledInstruction {
                                program_id_index: instruction
                                    .program_id_index
                                    .try_into()
                                    .map_err(|_| "failed to parse program_id_index")?,
                                accounts: instruction.accounts,
                                data: instruction.data,
                            },
                            stack_height: instruction.stack_height,
                        })
                    })
                    .collect::<ConvertResult<Vec<_>>>()?,
            })
        })
        .collect()
}

fn create_token_balances(
    balances: Vec<proto::TokenBalance>,
) -> ConvertResult<Vec<TransactionTokenBalance>> {
    balances
        .into_iter()
        .map(|balance| {
            let amount = balance
                .ui_token_amount
                .ok_or("failed to get UiTokenAmount")?;
            Ok(TransactionTokenBalance {
                account_index: balance
                    .account_index
                    .try_into()
                    .map_err(|_| "failed to parse account_index")?,
                mint: balance.mint,
                ui_token_amount: UiTokenAmount {
                    ui_amount: (amount.ui_amount != 0f64).then_some(amount.ui_amount),
                    decimals: amount
                        .decimals
                        .try_into()
                        .map_err(|_| "failed to parse decimals")?,
                    amount: amount.amount,
                    ui_amount_string: amount.ui_amount_string,
                },
                owner: balance.owner,
                program_id: balance.program_id,
            })
        })
        .collect()
}

fn create_rewards(rewards: Vec<proto::Reward>) -> ConvertResult<Vec<Reward>> {
    rewards
        .into_iter()
        .map(|reward| {
            Ok(Reward {
                pubkey: reward.pubkey,
                lamports: reward.lamports,
                post_balance: reward.post_balance,
                reward_type: match proto::RewardType::try_from(reward.reward_type)
                    .map_err(|_| "failed to parse reward_type")?
                {
                    proto::RewardType::Unspecified => None,
                    proto::RewardType::Fee => Some(RewardType::Fee),
                    proto::RewardType::Rent => Some(RewardType::Rent),
                    proto::RewardType::Staking => Some(RewardType::Staking),
                    proto::RewardType::Voting => Some(RewardType::Voting),
                    proto::RewardType::DeactivatedStake => None,
                },
                commission: parse_optional_number(&reward.commission, "reward commission")?,
                commission_bps: parse_optional_number(
                    &reward.commission_bps,
                    "reward commission_bps",
                )?,
            })
        })
        .collect()
}

fn parse_optional_number<T: std::str::FromStr>(
    value: &str,
    what: &'static str,
) -> ConvertResult<Option<T>> {
    if value.is_empty() {
        return Ok(None);
    }
    value.parse().map(Some).map_err(|_| what)
}

fn create_pubkey_vec(pubkeys: Vec<Vec<u8>>) -> ConvertResult<Vec<Pubkey>> {
    pubkeys.iter().map(|pubkey| create_pubkey(pubkey)).collect()
}

fn create_pubkey(pubkey: &[u8]) -> ConvertResult<Pubkey> {
    Pubkey::try_from(pubkey).map_err(|_| "failed to parse Pubkey")
}

fn create_hash(hash: &[u8]) -> ConvertResult<Hash> {
    <[u8; HASH_BYTES]>::try_from(hash)
        .map(Hash::new_from_array)
        .map_err(|_| "failed to parse Hash")
}
