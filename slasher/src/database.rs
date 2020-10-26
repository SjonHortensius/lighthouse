use crate::{AttesterRecord, Config, Error, ProposerSlashingStatus, SlashingStatus};
use lmdb::{Database, DatabaseFlags, Environment, RwTransaction, Transaction, WriteFlags};
use ssz::{Decode, Encode};
use std::marker::PhantomData;
use std::sync::Arc;
use types::{
    Epoch, EthSpec, Hash256, IndexedAttestation, ProposerSlashing, SignedBeaconBlockHeader, Slot,
};

/// Map from `(validator_index, target_epoch)` to `AttesterRecord`.
const ATTESTER_DB: &str = "attester";
/// Map from `indexed_attestation_hash` to `IndexedAttestation`.
const INDEXED_ATTESTATION_DB: &str = "indexed_attestations";
const MIN_TARGETS_DB: &str = "min_targets";
const MAX_TARGETS_DB: &str = "max_targets";
/// Map from `(validator_index, slot)` to `SignedBeaconBlockHeader`.
const PROPOSER_DB: &str = "proposer";

/// The number of DBs for LMDB to use (equal to the number of DBs defined above).
const LMDB_MAX_DBS: u32 = 5;
/// The size of the in-memory map for LMDB (larger than the maximum size of the database).
// FIXME(sproul): make this user configurable
const LMDB_MAP_SIZE: usize = 256 * (1 << 30); // 256GiB

const ATTESTER_KEY_SIZE: usize = 16;
const PROPOSER_KEY_SIZE: usize = 16;

#[derive(Debug)]
pub struct SlasherDB<E: EthSpec> {
    pub(crate) env: Environment,
    pub(crate) indexed_attestation_db: Database,
    pub(crate) attester_db: Database,
    pub(crate) min_targets_db: Database,
    pub(crate) max_targets_db: Database,
    pub(crate) proposer_db: Database,
    config: Arc<Config>,
    _phantom: PhantomData<E>,
}

#[derive(Debug)]
pub struct AttesterKey {
    data: [u8; ATTESTER_KEY_SIZE],
}

impl AttesterKey {
    pub fn new(validator_index: u64, target_epoch: Epoch, config: &Config) -> Self {
        let mut data = [0; ATTESTER_KEY_SIZE];
        let epoch_offset = target_epoch.as_usize() % config.history_length;
        data[0..8].copy_from_slice(&validator_index.to_be_bytes());
        data[8..ATTESTER_KEY_SIZE].copy_from_slice(&epoch_offset.to_be_bytes());
        AttesterKey { data }
    }
}

impl AsRef<[u8]> for AttesterKey {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

#[derive(Debug)]
pub struct ProposerKey {
    data: [u8; PROPOSER_KEY_SIZE],
}

impl ProposerKey {
    pub fn new(validator_index: u64, slot: Slot) -> Self {
        let mut data = [0; PROPOSER_KEY_SIZE];
        data[0..8].copy_from_slice(&validator_index.to_be_bytes());
        data[8..ATTESTER_KEY_SIZE].copy_from_slice(&slot.as_u64().to_be_bytes());
        ProposerKey { data }
    }
}

impl AsRef<[u8]> for ProposerKey {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl<E: EthSpec> SlasherDB<E> {
    pub fn open(config: Arc<Config>) -> Result<Self, Error> {
        // TODO: open_with_permissions
        std::fs::create_dir_all(&config.database_path)?;
        let env = Environment::new()
            .set_max_dbs(LMDB_MAX_DBS)
            .set_map_size(LMDB_MAP_SIZE)
            .open(&config.database_path)?;
        let indexed_attestation_db =
            env.create_db(Some(INDEXED_ATTESTATION_DB), Self::db_flags())?;
        let attester_db = env.create_db(Some(ATTESTER_DB), Self::db_flags())?;
        let min_targets_db = env.create_db(Some(MIN_TARGETS_DB), Self::db_flags())?;
        let max_targets_db = env.create_db(Some(MAX_TARGETS_DB), Self::db_flags())?;
        let proposer_db = env.create_db(Some(PROPOSER_DB), Self::db_flags())?;
        Ok(Self {
            env,
            indexed_attestation_db,
            attester_db,
            min_targets_db,
            max_targets_db,
            proposer_db,
            config,
            _phantom: PhantomData,
        })
    }

    pub fn db_flags() -> DatabaseFlags {
        DatabaseFlags::default()
    }

    pub fn write_flags() -> WriteFlags {
        WriteFlags::default()
    }

    pub fn begin_rw_txn(&self) -> Result<RwTransaction<'_>, Error> {
        Ok(self.env.begin_rw_txn()?)
    }

    pub fn store_indexed_attestation(
        &self,
        txn: &mut RwTransaction<'_>,
        indexed_attestation_hash: Hash256,
        indexed_attestation: &IndexedAttestation<E>,
    ) -> Result<(), Error> {
        let data = indexed_attestation.as_ssz_bytes();

        txn.put(
            self.indexed_attestation_db,
            &indexed_attestation_hash.as_bytes(),
            &data,
            Self::write_flags(),
        )?;
        Ok(())
    }

    pub fn get_indexed_attestation(
        &self,
        txn: &mut RwTransaction<'_>,
        indexed_attestation_hash: Hash256,
    ) -> Result<IndexedAttestation<E>, Error> {
        match txn.get(self.indexed_attestation_db, &indexed_attestation_hash) {
            Ok(bytes) => Ok(IndexedAttestation::from_ssz_bytes(bytes)?),
            Err(lmdb::Error::NotFound) => Err(Error::MissingIndexedAttestation {
                root: indexed_attestation_hash,
            }),
            Err(e) => Err(e.into()),
        }
    }

    pub fn check_and_update_attester_record(
        &self,
        txn: &mut RwTransaction<'_>,
        validator_index: u64,
        attestation: &IndexedAttestation<E>,
        record: AttesterRecord,
    ) -> Result<SlashingStatus<E>, Error> {
        // See if there's an existing attestation for this attester.
        if let Some(existing_record) =
            self.get_attester_record(txn, validator_index, attestation.data.target.epoch)?
        {
            // If the existing attestation data is identical, then this attestation is not
            // slashable and no update is required.
            if existing_record.attestation_data_hash == record.attestation_data_hash {
                return Ok(SlashingStatus::NotSlashable);
            }

            // Otherwise, load the indexed attestation so we can confirm that it's slashable.
            let existing_attestation =
                self.get_indexed_attestation(txn, existing_record.indexed_attestation_hash)?;
            if attestation.is_double_vote(&existing_attestation) {
                Ok(SlashingStatus::DoubleVote(Box::new(existing_attestation)))
            } else {
                // FIXME(sproul): this could be an Err
                Ok(SlashingStatus::NotSlashable)
            }
        }
        // If no attestation exists, insert a record for this validator.
        else {
            txn.put(
                self.attester_db,
                &AttesterKey::new(validator_index, attestation.data.target.epoch, &self.config),
                &record.as_ssz_bytes(),
                Self::write_flags(),
            )?;
            Ok(SlashingStatus::NotSlashable)
        }
    }

    pub fn get_attestation_for_validator(
        &self,
        txn: &mut RwTransaction<'_>,
        validator_index: u64,
        target: Epoch,
    ) -> Result<Option<IndexedAttestation<E>>, Error> {
        if let Some(record) = self.get_attester_record(txn, validator_index, target)? {
            Ok(Some(self.get_indexed_attestation(
                txn,
                record.indexed_attestation_hash,
            )?))
        } else {
            Ok(None)
        }
    }

    pub fn get_attester_record(
        &self,
        txn: &mut RwTransaction<'_>,
        validator_index: u64,
        target: Epoch,
    ) -> Result<Option<AttesterRecord>, Error> {
        let attester_key = AttesterKey::new(validator_index, target, &self.config);
        match txn.get(self.attester_db, &attester_key) {
            Ok(bytes) => Ok(Some(AttesterRecord::from_ssz_bytes(bytes)?)),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_block_proposal(
        &self,
        txn: &mut RwTransaction<'_>,
        proposer_index: u64,
        slot: Slot,
    ) -> Result<Option<SignedBeaconBlockHeader>, Error> {
        let proposer_key = ProposerKey::new(proposer_index, slot);
        match txn.get(self.proposer_db, &proposer_key) {
            Ok(bytes) => Ok(Some(SignedBeaconBlockHeader::from_ssz_bytes(bytes)?)),
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn check_or_insert_block_proposal(
        &self,
        txn: &mut RwTransaction<'_>,
        block_header: SignedBeaconBlockHeader,
    ) -> Result<ProposerSlashingStatus, Error> {
        let proposer_index = block_header.message.proposer_index;
        let slot = block_header.message.slot;

        if let Some(existing_block) = self.get_block_proposal(txn, proposer_index, slot)? {
            if existing_block == block_header {
                Ok(ProposerSlashingStatus::NotSlashable)
            } else {
                Ok(ProposerSlashingStatus::DoubleVote(Box::new(
                    ProposerSlashing {
                        signed_header_1: existing_block,
                        signed_header_2: block_header,
                    },
                )))
            }
        } else {
            txn.put(
                self.proposer_db,
                &ProposerKey::new(proposer_index, slot),
                &block_header.as_ssz_bytes(),
                Self::write_flags(),
            )?;
            Ok(ProposerSlashingStatus::NotSlashable)
        }
    }
}

// FIXME(sproul): consider using this to avoid allocations
#[allow(unused)]
fn hash256_from_slice(data: &[u8]) -> Result<Hash256, Error> {
    if data.len() == 32 {
        Ok(Hash256::from_slice(data))
    } else {
        Err(Error::AttesterRecordCorrupt { length: data.len() })
    }
}
