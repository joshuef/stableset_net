use std::collections::HashSet;

use crate::Client;
use bls::SecretKey;
use bytes::Bytes;
use libp2p::kad::{Quorum, Record};
use sn_client::{
    networking::{GetRecordCfg, NetworkError, PutRecordCfg, VerificationKind},
    transfers::HotWallet,
};
use sn_protocol::storage::{RetryStrategy, Scratchpad, ScratchpadAddress};
use sn_protocol::{
    storage::{try_deserialize_record, try_serialize_record, RecordKind},
    NetworkAddress,
};
use tracing::info;

use super::data::PutError;

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("Could not generate Account secret key from entropy: {0:?}")]
    Bls(#[from] bls::Error),
    #[error("No Account has been defined. Use `client.with_account_entropy` to define one.")]
    NoAccountPacketDefined,
    #[error("Scratchpad found at {0:?} was not a valid record.")]
    CouldNotDeserializeAccountScratchPad(ScratchpadAddress),
    #[error("Protocol: {0}")]
    Protocol(#[from] sn_protocol::Error),
    #[error("Network: {0}")]
    Network(#[from] NetworkError),
}

impl Client {
    /// Add an account secret key to the client
    ///
    /// The secret key is derived from the supplied entropy bytes.
    pub fn with_account_entropy(mut self, bytes: Bytes) -> Result<Self, AccountError> {
        // simple hash as XORNAME_LEN == SK_LENs
        let xorname = xor_name::XorName::from_content(&bytes);
        // before generating the sk from these bytes.
        self.account_secret_key = Some(SecretKey::from_bytes(xorname.0)?);

        Ok(self)
    }

    /// Retrieves and returns a decrypted account packet if one exists.
    pub async fn get_decrypt_account(&self) -> Result<Option<Bytes>, AccountError> {
        let Some(account_secret_key) = self.account_secret_key.as_ref() else {
            return Err(AccountError::NoAccountPacketDefined);
        };

        let pad = self.get_account_scratchpad_from_network().await?;

        Ok(pad.decrypt_data(account_secret_key)?)
    }

    /// Gets the account Scratchpad from a provided client public key
    async fn get_account_scratchpad_from_network(&self) -> Result<Scratchpad, AccountError> {
        // let account_packet = self.account_packet.as_ref()?;
        let Some(account_secret_key) = self.account_secret_key.as_ref() else {
            return Err(AccountError::NoAccountPacketDefined);
        };

        let client_pk = account_secret_key.public_key();

        let scratch_address = ScratchpadAddress::new(client_pk);
        let network_address = NetworkAddress::from_scratchpad_address(scratch_address);
        let scratch_key = network_address.to_record_key();

        let get_cfg = GetRecordCfg {
            get_quorum: Quorum::Majority,
            retry_strategy: None,
            target_record: None,
            expected_holders: HashSet::new(),
        };

        let record = self
            .network
            .get_record_from_network(scratch_key, &get_cfg)
            .await?;

        let pad = try_deserialize_record::<Scratchpad>(&record)
            .map_err(|_| AccountError::CouldNotDeserializeAccountScratchPad(scratch_address))?;

        Ok(pad)
    }

    /// Put data into the client's AccountPacket
    ///
    /// Returns Ok(None) early if no account packet is defined.
    ///
    /// Pays for a new AccountPacket if none yet created for the client. Returns the current version
    /// of the data on success.
    pub async fn write_bytes_to_account_packet_if_defined(
        &mut self,
        data: Bytes,
        wallet: &mut HotWallet,
    ) -> Result<Option<u64>, PutError> {
        // Exit early if no account packet defined
        let Some(client_sk) = self.account_secret_key.as_ref() else {
            return Ok(None);
        };

        let client_pk = client_sk.public_key();

        let pad_res = self.get_account_scratchpad_from_network().await;

        let mut is_new = true;
        let mut scratch = if let Ok(existing_data) = pad_res {
            tracing::info!("Scratchpad already exists, returning existing data");

            info!(
                "scratch already exists, is version {:?}",
                existing_data.count()
            );

            is_new = false;
            existing_data
        } else {
            tracing::warn!("new scratch");
            Scratchpad::new(client_pk)
        };

        let next_count = scratch.update_and_sign(data, client_sk);
        let scratch_address = scratch.network_address();
        let scratch_key = scratch_address.to_record_key();

        let record = if is_new {
            self.pay(
                [&scratch_address].iter().filter_map(|f| f.as_xorname()),
                wallet,
            )
            .await
            .expect("TODO: handle error");

            let (payment, _payee) = self.get_recent_payment_for_addr(
                &scratch_address
                    .as_xorname()
                    .ok_or(PutError::AccountPacketXorName)?,
                wallet,
            )?;

            Record {
                key: scratch_key,
                value: try_serialize_record(&(payment, scratch), RecordKind::ScratchpadWithPayment)
                    .map_err(|_| PutError::Serialization)?
                    .to_vec(),
                publisher: None,
                expires: None,
            }
        } else {
            Record {
                key: scratch_key,
                value: try_serialize_record(&scratch, RecordKind::Scratchpad)
                    .map_err(|_| PutError::Serialization)?
                    .to_vec(),
                publisher: None,
                expires: None,
            }
        };

        let put_cfg = PutRecordCfg {
            put_quorum: Quorum::Majority,
            retry_strategy: Some(RetryStrategy::Balanced),
            use_put_record_to: None,
            verification: Some((
                VerificationKind::Network,
                GetRecordCfg {
                    get_quorum: Quorum::Majority,
                    retry_strategy: None,
                    target_record: None,
                    expected_holders: HashSet::new(),
                },
            )),
        };

        self.network.put_record(record, &put_cfg).await?;

        Ok(Some(next_count))
    }
}
