use crate::data::rpc::RpcPool;
use solana_sdk::{message::VersionedMessage, pubkey::Pubkey, transaction::VersionedTransaction};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("address lookup table data is malformed: {0}")]
    MalformedAlt(String),
    #[error("failed to fetch address lookup table from chain: {0}")]
    AltFetchFailed(String),
    #[error("transaction has unexpected signer layout")]
    Signers,
    #[error("transaction payer differs from configured signer")]
    Payer,
    #[error("invalid allowed program id: {0}")]
    ProgramId(String),
    #[error("unexpected program: {0}")]
    Program(String),
    #[error("instruction references an invalid account index")]
    AccountIndex,
}

/// Caches resolved ALT address lists to avoid repeated RPC fetches for the
/// same ALT account within a session. Each ALT account is fetched at most
/// once; subsequent references to the same table use the cached result.
pub struct AltResolver<'a> {
    rpc: &'a RpcPool,
    cache: HashMap<Pubkey, Vec<Pubkey>>,
}

impl<'a> AltResolver<'a> {
    pub fn new(rpc: &'a RpcPool) -> Self {
        Self {
            rpc,
            cache: HashMap::new(),
        }
    }

    /// Fetch and decode an Address Lookup Table account from the chain.
    ///
    /// ALT account data format (Solana v1.18+):
    /// ```text
    /// [0]       u8   discriminator = 0x01
    /// [1]       u8   status (1 = active)
    /// [2]       u8   padding
    /// [3..11]   u64  deactivation_slot (little-endian)
    /// [11..]    [u8; 32] N consecutive 32-byte pubkeys
    /// ```
    ///
    /// Returns the list of addresses stored in the table, or an error if the
    /// account data is malformed or missing.
    async fn fetch_alt_addresses(&mut self, alt_key: &Pubkey) -> Result<&Vec<Pubkey>, PolicyError> {
        if self.cache.contains_key(alt_key) {
            return Ok(self.cache.get(alt_key).unwrap());
        }
        let key_str = alt_key.to_string();
        let accounts = self
            .rpc
            .fetch_accounts_base64(&[&key_str])
            .await
            .map_err(|e| PolicyError::AltFetchFailed(format!("{alt_key}: {e}")))?;
        let data =
            accounts.into_iter().next().flatten().ok_or_else(|| {
                PolicyError::AltFetchFailed(format!("{alt_key}: account not found"))
            })?;

        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &data)
            .map_err(|e| PolicyError::MalformedAlt(format!("{alt_key}: invalid base64: {e}")))?;

        // Minimum size: 3 bytes discriminator + 8 bytes deactivation_slot = 11 bytes.
        // After that, every 32 bytes is one address.
        if raw.len() < 11 {
            return Err(PolicyError::MalformedAlt(format!(
                "{alt_key}: data too short ({} bytes, minimum 11)",
                raw.len()
            )));
        }
        // Validate discriminator: 0x01, 0x00, 0x00
        if raw[0] != 0x01 || raw[1] != 0x00 || raw[2] != 0x00 {
            return Err(PolicyError::MalformedAlt(format!(
                "{alt_key}: invalid discriminator [{}, {}, {}]",
                raw[0], raw[1], raw[2]
            )));
        }
        let addr_bytes = &raw[11..];
        if addr_bytes.len() % 32 != 0 {
            return Err(PolicyError::MalformedAlt(format!(
                "{alt_key}: address data length {} is not a multiple of 32",
                addr_bytes.len()
            )));
        }
        let n = addr_bytes.len() / 32;
        let mut addrs = Vec::with_capacity(n);
        for i in 0..n {
            let start = i * 32;
            let slice: [u8; 32] = addr_bytes[start..start + 32].try_into().map_err(|_| {
                PolicyError::MalformedAlt(format!("{alt_key}: addr[{i}] slice error"))
            })?;
            addrs.push(Pubkey::new_from_array(slice));
        }
        self.cache.insert(*alt_key, addrs);
        Ok(self.cache.get(alt_key).unwrap())
    }
}

/// Resolve all account keys from a V0 message, including those referenced
/// by Address Lookup Tables. This performs REAL ALT resolution by fetching
/// the ALT accounts from the chain.
///
/// The Solana v0 transaction format defines the following account key layout:
///   [0 .. static_len)                          static account keys
///   [static_len .. static_len + writable_len)  loaded writable addresses (from ALTs)
///   [static_len + writable_len ..)              loaded readonly addresses (from ALTs)
///
/// Instruction account indexes reference this combined list:
///   - indexes [0, static_len) reference static keys
///   - indexes [static_len, static_len + writable_len) reference loaded writable
///   - indexes [static_len + writable_len, total) reference loaded readonly
///
/// Returns (all_account_keys, num_required_signatures, instructions).
async fn resolve_v0_accounts(
    m: &solana_sdk::message::v0::Message,
    resolver: &mut AltResolver<'_>,
) -> Result<
    (
        Vec<Pubkey>,
        u8,
        Vec<solana_sdk::instruction::CompiledInstruction>,
    ),
    PolicyError,
> {
    let static_keys: Vec<Pubkey> = m.account_keys.clone();

    // Collect all addresses from ALTs in order: writable first, then readonly.
    // Per the Solana spec, the order of ALTs in address_table_lookups defines
    // the order of loaded writable addresses (all writable from first ALT,
    // then all writable from second ALT, etc.), followed by all readonly
    // addresses in the same ALT order.
    let mut loaded_writable: Vec<Pubkey> = Vec::new();
    let mut loaded_readonly: Vec<Pubkey> = Vec::new();

    for lookup in &m.address_table_lookups {
        let addrs = resolver.fetch_alt_addresses(&lookup.account_key).await?;

        // Validate indexes: each index must be within the ALT's address range.
        let table_len = addrs.len();
        for &idx in &lookup.writable_indexes {
            if idx as usize >= table_len {
                return Err(PolicyError::MalformedAlt(format!(
                    "ALT {} writable index {} exceeds table length {}",
                    lookup.account_key, idx, table_len
                )));
            }
            loaded_writable.push(addrs[idx as usize]);
        }
        for &idx in &lookup.readonly_indexes {
            if idx as usize >= table_len {
                return Err(PolicyError::MalformedAlt(format!(
                    "ALT {} readonly index {} exceeds table length {}",
                    lookup.account_key, idx, table_len
                )));
            }
            loaded_readonly.push(addrs[idx as usize]);
        }
    }

    // Build the complete account key list.
    let mut all_keys = static_keys;
    all_keys.extend(loaded_writable);
    all_keys.extend(loaded_readonly);

    Ok((
        all_keys,
        m.header.num_required_signatures,
        m.instructions.clone(),
    ))
}

pub async fn validate_provider_transaction(
    tx: &VersionedTransaction,
    signer: &Pubkey,
    allowed: &[String],
    resolver: &mut AltResolver<'_>,
) -> Result<(), PolicyError> {
    let allow: HashSet<Pubkey> = allowed
        .iter()
        .map(|s| s.parse().map_err(|_| PolicyError::ProgramId(s.clone())))
        .collect::<Result<_, _>>()?;
    let (keys, required, instructions): (
        Vec<Pubkey>,
        u8,
        Vec<solana_sdk::instruction::CompiledInstruction>,
    ) = match &tx.message {
        VersionedMessage::Legacy(m) => (
            m.account_keys.clone(),
            m.header.num_required_signatures,
            m.instructions.clone(),
        ),
        VersionedMessage::V0(m) => {
            let (keys, required, instructions) = resolve_v0_accounts(m, resolver).await?;
            (keys, required, instructions)
        }
    };
    if required != 1 {
        return Err(PolicyError::Signers);
    }
    if keys.first() != Some(signer) {
        return Err(PolicyError::Payer);
    }
    for ix in instructions {
        let program = keys
            .get(ix.program_id_index as usize)
            .ok_or(PolicyError::AccountIndex)?;
        if !allow.contains(program) {
            return Err(PolicyError::Program(program.to_string()));
        }
        if ix.accounts.iter().any(|i| keys.get(*i as usize).is_none()) {
            return Err(PolicyError::AccountIndex);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        instruction::Instruction, message::Message, signature::Keypair, signer::Signer,
        transaction::Transaction,
    };
    fn make_tx(instructions: &[Instruction], payer: &Keypair) -> VersionedTransaction {
        Transaction::new_unsigned(Message::new(instructions, Some(&payer.pubkey()))).into()
    }
    fn prog_instruction(prog: Pubkey) -> Instruction {
        Instruction::new_with_bytes(prog, &[], vec![])
    }
    fn dummy_rpc() -> RpcPool {
        RpcPool::new(
            vec!["http://127.0.0.1:8899".into()],
            std::time::Duration::from_secs(5),
        )
        .unwrap()
    }

    #[test]
    fn refuses_unknown_program() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p)], &payer);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            assert!(matches!(
                validate_provider_transaction(&v, &payer.pubkey(), &[], &mut resolver).await,
                Err(PolicyError::Program(_))
            ));
        });
    }
    #[test]
    fn allows_program_in_allowlist() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p)], &payer);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            assert!(validate_provider_transaction(
                &v,
                &payer.pubkey(),
                &[p.to_string()],
                &mut resolver
            )
            .await
            .is_ok());
        });
    }
    #[test]
    fn refuses_payer_mismatch() {
        let payer = Keypair::new();
        let wrong = Keypair::new();
        let p = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p)], &payer);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            assert!(matches!(
                validate_provider_transaction(&v, &wrong.pubkey(), &[p.to_string()], &mut resolver)
                    .await,
                Err(PolicyError::Payer)
            ));
        });
    }
    #[test]
    fn refuses_multiple_signers() {
        let payer = Keypair::new();
        let extra = Keypair::new();
        let p = Pubkey::new_unique();
        let ix = Instruction::new_with_bytes(
            p,
            &[],
            vec![
                solana_sdk::instruction::AccountMeta::new(payer.pubkey(), true),
                solana_sdk::instruction::AccountMeta::new(extra.pubkey(), true),
            ],
        );
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let v: VersionedTransaction = Transaction::new_unsigned(msg).into();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            assert!(matches!(
                validate_provider_transaction(&v, &payer.pubkey(), &[p.to_string()], &mut resolver)
                    .await,
                Err(PolicyError::Signers)
            ));
        });
    }
    #[test]
    fn allows_v0_transaction_without_alt() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let v0 = solana_sdk::message::v0::Message::try_compile(
            &payer.pubkey(),
            &[Instruction::new_with_bytes(p, &[], vec![])],
            &[],
            Default::default(),
        )
        .unwrap();
        let vt =
            VersionedTransaction::try_new(solana_sdk::message::VersionedMessage::V0(v0), &[&payer])
                .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            assert!(
                validate_provider_transaction(
                    &vt,
                    &payer.pubkey(),
                    &[p.to_string()],
                    &mut resolver
                )
                .await
                .is_ok(),
                "V0 without ALT should be accepted"
            );
        });
    }

    #[test]
    fn v0_with_alt_referencing_nonexistent_account_is_rejected() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let v0 = solana_sdk::message::v0::Message::try_compile(
            &payer.pubkey(),
            &[Instruction::new_with_bytes(p, &[], vec![])],
            &[],
            Default::default(),
        )
        .unwrap();
        let mut vt =
            VersionedTransaction::try_new(solana_sdk::message::VersionedMessage::V0(v0), &[&payer])
                .unwrap();
        // Add an ALT lookup referencing a key NOT in account_keys.
        // This will fail at the RPC fetch step since the ALT account doesn't exist.
        if let solana_sdk::message::VersionedMessage::V0(ref mut m) = vt.message {
            m.address_table_lookups
                .push(solana_sdk::message::v0::MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: vec![0],
                    readonly_indexes: vec![],
                });
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            let result = validate_provider_transaction(
                &vt,
                &payer.pubkey(),
                &[p.to_string()],
                &mut resolver,
            )
            .await;
            // Should fail: either AltFetchFailed (RPC can't reach the ALT) or
            // MalformedAlt (if somehow parsed but invalid).
            assert!(
                matches!(
                    result,
                    Err(PolicyError::AltFetchFailed(_)) | Err(PolicyError::MalformedAlt(_))
                ),
                "expected AltFetchFailed or MalformedAlt, got {:?}",
                result
            );
        });
    }
    #[test]
    fn refuses_invalid_account_index() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let ix = solana_sdk::instruction::Instruction {
            program_id: p,
            accounts: vec![solana_sdk::instruction::AccountMeta::new(
                Pubkey::new_unique(),
                false,
            )],
            data: vec![],
        };
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let mut v: VersionedTransaction = Transaction::new_unsigned(msg).into();
        if let solana_sdk::message::VersionedMessage::Legacy(ref mut m) = v.message {
            for inst in &mut m.instructions {
                inst.program_id_index = 0xFF;
            }
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            assert!(matches!(
                validate_provider_transaction(&v, &payer.pubkey(), &[p.to_string()], &mut resolver)
                    .await,
                Err(PolicyError::AccountIndex)
            ));
        });
    }
    #[test]
    fn allows_multiple_instructions_all_in_allowlist() {
        let payer = Keypair::new();
        let p1 = Pubkey::new_unique();
        let p2 = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p1), prog_instruction(p2)], &payer);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            assert!(validate_provider_transaction(
                &v,
                &payer.pubkey(),
                &[p1.to_string(), p2.to_string()],
                &mut resolver
            )
            .await
            .is_ok());
        });
    }

    // --- ALT resolution unit tests (using in-memory mock data) ---

    /// Build a V0 transaction that uses an ALT with known addresses.
    /// The transaction has:
    /// - 1 static key: payer (signer)
    /// - 1 ALT lookup referencing 2 writable and 1 readonly from the table
    /// - 1 instruction referencing payer (index 0) and loaded writable (index 2)
    fn build_v0_with_alt(
        payer: &Keypair,
        program: Pubkey,
        writable_indexes: Vec<u8>,
        readonly_indexes: Vec<u8>,
    ) -> VersionedTransaction {
        use solana_sdk::message::v0::MessageAddressTableLookup;

        // Static keys: [payer]
        let mut static_keys = vec![payer.pubkey()];
        // The program must be in static keys for the instruction to reference it
        static_keys.push(program);

        // Build a V0 message manually (try_compile doesn't support ALTs in SDK)
        let header = solana_sdk::message::MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        };
        let instructions = vec![solana_sdk::instruction::CompiledInstruction {
            program_id_index: 1, // program is at index 1 in static_keys
            accounts: vec![0],   // payer at index 0
            data: vec![],
        }];

        let v0_msg = solana_sdk::message::v0::Message {
            header,
            account_keys: static_keys,
            recent_blockhash: solana_sdk::hash::Hash::default(),
            instructions,
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: Pubkey::new_unique(), // placeholder; not validated against chain here
                writable_indexes,
                readonly_indexes,
            }],
        };

        VersionedTransaction::try_new(solana_sdk::message::VersionedMessage::V0(v0_msg), &[payer])
            .unwrap()
    }

    #[test]
    fn alt_fetch_failure_rejects_transaction() {
        // An ALT lookup referencing a non-existent account should fail.
        let payer = Keypair::new();
        let program = Pubkey::new_unique();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);

            // Build a V0 tx with an ALT lookup for a random pubkey (not on chain)
            let vt = build_v0_with_alt(&payer, program, vec![0], vec![]);
            let result = validate_provider_transaction(
                &vt,
                &payer.pubkey(),
                &[program.to_string()],
                &mut resolver,
            )
            .await;
            assert!(
                matches!(result, Err(PolicyError::AltFetchFailed(_))),
                "expected AltFetchFailed, got {:?}",
                result
            );
        });
    }

    #[test]
    fn alt_index_out_of_bounds_rejects() {
        // If the ALT has 2 addresses but the lookup references index 5, it should fail.
        // We simulate this by directly testing the resolve logic.
        let alt_addrs = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let alt_key = Pubkey::new_unique();

        let static_keys = vec![Pubkey::new_unique()];
        let header = solana_sdk::message::MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        };
        let v0_msg = solana_sdk::message::v0::Message {
            header,
            account_keys: static_keys,
            recent_blockhash: solana_sdk::hash::Hash::default(),
            instructions: vec![],
            address_table_lookups: vec![solana_sdk::message::v0::MessageAddressTableLookup {
                account_key: alt_key,
                writable_indexes: vec![5], // out of bounds (table has 2 entries)
                readonly_indexes: vec![],
            }],
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Manually populate the cache with the ALT data
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            resolver.cache.insert(alt_key, alt_addrs);

            let result = resolve_v0_accounts(&v0_msg, &mut resolver).await;
            assert!(
                matches!(result, Err(PolicyError::MalformedAlt(ref e)) if e.contains("exceeds table length")),
                "expected MalformedAlt for out-of-bounds index, got {:?}",
                result
            );
        });
    }

    #[test]
    fn loaded_writable_addresses_appear_in_correct_range() {
        // Verify that loaded writable addresses are placed after static keys
        // and before loaded readonly addresses.
        let payer = Keypair::new();
        let program = Pubkey::new_unique();
        let addr_w = Pubkey::new_unique(); // writable loaded address
        let addr_r = Pubkey::new_unique(); // readonly loaded address
        let alt_key = Pubkey::new_unique();

        let static_keys = vec![payer.pubkey(), program];
        let header = solana_sdk::message::MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        };
        // Instruction: program_id at index 1, account[0] = index 2 (loaded writable)
        let instructions = vec![solana_sdk::instruction::CompiledInstruction {
            program_id_index: 1,
            accounts: vec![2], // index 2 = static_len(2) + writable_offset(0) = first writable loaded
            data: vec![],
        }];
        let v0_msg = solana_sdk::message::v0::Message {
            header,
            account_keys: static_keys,
            recent_blockhash: solana_sdk::hash::Hash::default(),
            instructions,
            address_table_lookups: vec![solana_sdk::message::v0::MessageAddressTableLookup {
                account_key: alt_key,
                writable_indexes: vec![0], // index 0 in ALT -> addr_w
                readonly_indexes: vec![1], // index 1 in ALT -> addr_r
            }],
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            // Manually populate cache: ALT has [addr_w, addr_r]
            resolver.cache.insert(alt_key, vec![addr_w, addr_r]);

            let (keys, _required, _) = resolve_v0_accounts(&v0_msg, &mut resolver).await.unwrap();

            // Expected layout:
            // [0] payer (static)
            // [1] program (static)
            // [2] addr_w (loaded writable)
            // [3] addr_r (loaded readonly)
            assert_eq!(keys.len(), 4);
            assert_eq!(keys[0], payer.pubkey());
            assert_eq!(keys[1], program);
            assert_eq!(keys[2], addr_w);
            assert_eq!(keys[3], addr_r);

            // Instruction account index 2 should resolve to addr_w
            assert_eq!(keys[2], addr_w);
        });
    }

    #[test]
    fn multiple_lookups_concatenate_writable_then_readonly() {
        // Two ALTs: first has 1 writable, second has 1 writable + 1 readonly.
        // Writable addresses should be [alt1_w0, alt2_w0], then readonly [alt2_r0].
        let alt1_key = Pubkey::new_unique();
        let alt2_key = Pubkey::new_unique();
        let addr_w1 = Pubkey::new_unique();
        let addr_w2 = Pubkey::new_unique();
        let addr_r1 = Pubkey::new_unique();

        let static_keys = vec![Pubkey::new_unique()];
        let header = solana_sdk::message::MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        };
        let v0_msg = solana_sdk::message::v0::Message {
            header,
            account_keys: static_keys,
            recent_blockhash: solana_sdk::hash::Hash::default(),
            instructions: vec![],
            address_table_lookups: vec![
                solana_sdk::message::v0::MessageAddressTableLookup {
                    account_key: alt1_key,
                    writable_indexes: vec![0], // -> addr_w1
                    readonly_indexes: vec![],
                },
                solana_sdk::message::v0::MessageAddressTableLookup {
                    account_key: alt2_key,
                    writable_indexes: vec![0], // -> addr_w2
                    readonly_indexes: vec![1], // -> addr_r1
                },
            ],
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            resolver.cache.insert(alt1_key, vec![addr_w1]);
            resolver.cache.insert(alt2_key, vec![addr_w2, addr_r1]);

            let (keys, _, _) = resolve_v0_accounts(&v0_msg, &mut resolver).await.unwrap();

            // [0] static, [1] alt1_w0, [2] alt2_w0, [3] alt2_r0
            assert_eq!(keys.len(), 4);
            assert_eq!(keys[1], addr_w1);
            assert_eq!(keys[2], addr_w2);
            assert_eq!(keys[3], addr_r1);
        });
    }

    #[test]
    fn transaction_with_resolved_alt_passes_validation() {
        // End-to-end: a V0 tx with ALT-resolved accounts that references an
        // allowed program should pass validation.
        let payer = Keypair::new();
        let program = Pubkey::new_unique();
        let addr_w = Pubkey::new_unique();
        let alt_key = Pubkey::new_unique();

        let static_keys = vec![payer.pubkey(), program];
        let header = solana_sdk::message::MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        };
        // Instruction: program (index 1), accounts[0] = index 2 (loaded writable)
        let instructions = vec![solana_sdk::instruction::CompiledInstruction {
            program_id_index: 1,
            accounts: vec![2],
            data: vec![],
        }];
        let vt = VersionedTransaction {
            signatures: vec![],
            message: solana_sdk::message::VersionedMessage::V0(solana_sdk::message::v0::Message {
                header,
                account_keys: static_keys,
                recent_blockhash: solana_sdk::hash::Hash::default(),
                instructions,
                address_table_lookups: vec![solana_sdk::message::v0::MessageAddressTableLookup {
                    account_key: alt_key,
                    writable_indexes: vec![0],
                    readonly_indexes: vec![],
                }],
            }),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);
            resolver.cache.insert(alt_key, vec![addr_w]);

            let result = validate_provider_transaction(
                &vt,
                &payer.pubkey(),
                &[program.to_string()],
                &mut resolver,
            )
            .await;
            assert!(
                result.is_ok(),
                "V0+ALT with resolved accounts should pass: {:?}",
                result
            );
        });
    }

    #[test]
    fn alt_cache_avoids_duplicate_fetches() {
        // The cache should return the same result without re-fetching.
        let alt_key = Pubkey::new_unique();
        let addr = Pubkey::new_unique();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let rpc = dummy_rpc();
            let mut resolver = AltResolver::new(&rpc);

            // Pre-populate cache
            resolver.cache.insert(alt_key, vec![addr]);
            let addrs = resolver.fetch_alt_addresses(&alt_key).await.unwrap();
            assert_eq!(addrs.len(), 1);
            assert_eq!(addrs[0], addr);
            // Should still be in cache (no RPC call needed)
            assert!(resolver.cache.contains_key(&alt_key));
        });
    }
}
