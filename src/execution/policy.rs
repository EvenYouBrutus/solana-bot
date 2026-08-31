use solana_sdk::{message::VersionedMessage, pubkey::Pubkey, transaction::VersionedTransaction};
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("provider transaction uses address lookup tables; legacy-only policy is configured")]
    AddressLookup,
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
pub fn validate_provider_transaction(
    tx: &VersionedTransaction,
    signer: &Pubkey,
    allowed: &[String],
) -> Result<(), PolicyError> {
    let allow: HashSet<Pubkey> = allowed
        .iter()
        .map(|s| s.parse().map_err(|_| PolicyError::ProgramId(s.clone())))
        .collect::<Result<_, _>>()?;
    let (keys, required, instructions) = match &tx.message {
        VersionedMessage::Legacy(m) => (
            m.account_keys.as_slice(),
            m.header.num_required_signatures,
            m.instructions.as_slice(),
        ),
        VersionedMessage::V0(m) => {
            if !m.address_table_lookups.is_empty() {
                return Err(PolicyError::AddressLookup);
            }
            (
                m.account_keys.as_slice(),
                m.header.num_required_signatures,
                m.instructions.as_slice(),
            )
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
    #[test]
    fn refuses_unknown_program() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p)], &payer);
        assert!(matches!(
            validate_provider_transaction(&v, &payer.pubkey(), &[]),
            Err(PolicyError::Program(_))
        ));
    }
    #[test]
    fn allows_program_in_allowlist() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p)], &payer);
        assert!(validate_provider_transaction(&v, &payer.pubkey(), &[p.to_string()]).is_ok());
    }
    #[test]
    fn refuses_payer_mismatch() {
        let payer = Keypair::new();
        let wrong = Keypair::new();
        let p = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p)], &payer);
        assert!(matches!(
            validate_provider_transaction(&v, &wrong.pubkey(), &[p.to_string()]),
            Err(PolicyError::Payer)
        ));
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
        assert!(matches!(
            validate_provider_transaction(&v, &payer.pubkey(), &[p.to_string()]),
            Err(PolicyError::Signers)
        ));
    }
    #[test]
    fn refuses_address_lookup_table() {
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
        if let solana_sdk::message::VersionedMessage::V0(ref mut m) = vt.message {
            m.address_table_lookups
                .push(solana_sdk::message::v0::MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: vec![0],
                    readonly_indexes: vec![],
                });
        }
        assert!(matches!(
            validate_provider_transaction(&vt, &payer.pubkey(), &[p.to_string()]),
            Err(PolicyError::AddressLookup)
        ));
    }
    #[test]
    fn refuses_invalid_account_index() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        // Instruction references account index 255, but message only has 2 keys
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
        // Tamper: set program_id_index to point beyond account_keys
        if let solana_sdk::message::VersionedMessage::Legacy(ref mut m) = v.message {
            // Overwrite the program_id_index byte to 0xFF (beyond bounds)
            for inst in &mut m.instructions {
                inst.program_id_index = 0xFF;
            }
        }
        assert!(matches!(
            validate_provider_transaction(&v, &payer.pubkey(), &[p.to_string()]),
            Err(PolicyError::AccountIndex)
        ));
    }
    #[test]
    fn allows_multiple_instructions_all_in_allowlist() {
        let payer = Keypair::new();
        let p1 = Pubkey::new_unique();
        let p2 = Pubkey::new_unique();
        let v = make_tx(&[prog_instruction(p1), prog_instruction(p2)], &payer);
        assert!(validate_provider_transaction(
            &v,
            &payer.pubkey(),
            &[p1.to_string(), p2.to_string()]
        )
        .is_ok());
    }
}
