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
    #[test]
    fn refuses_unknown_program() {
        let payer = Keypair::new();
        let p = Pubkey::new_unique();
        let tx = Transaction::new_unsigned(Message::new(
            &[Instruction::new_with_bytes(p, &[], vec![])],
            Some(&payer.pubkey()),
        ));
        let v: VersionedTransaction = tx.into();
        assert!(matches!(
            validate_provider_transaction(&v, &payer.pubkey(), &[]),
            Err(PolicyError::Program(_))
        ));
    }
}
