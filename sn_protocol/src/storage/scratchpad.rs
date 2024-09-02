// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::ScratchpadAddress;
use crate::error::{Error, Result};
use crate::NetworkAddress;
use bls::{Ciphertext, PublicKey, SecretKey, Signature};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use xor_name::XorName;

/// Scratchpad, an mutable address for encrypted data
#[derive(
    Hash, Eq, PartialEq, PartialOrd, Ord, Clone, custom_debug::Debug, Serialize, Deserialize,
)]
pub struct Scratchpad {
    /// Network address. Omitted when serialising and
    /// calculated from the `encrypted_data` when deserialising.
    address: ScratchpadAddress,
    /// Contained data. This should be encrypted
    #[debug(skip)]
    encrypted_data: Bytes,
    /// Monotonically increasing counter to track the number of times this has been updated.
    counter: u64,
    /// Signature over Vec<counter>.extend(Xorname::from_content(encrypted_data).to_vec()) from the owning key.
    /// Required for scratchpad to be valid.
    signature: Option<Signature>,
}

impl Scratchpad {
    /// Creates a new instance of `Scratchpad`.
    pub fn new(owner: PublicKey) -> Self {
        Self {
            address: ScratchpadAddress::new(owner),
            encrypted_data: Bytes::new(),
            counter: 0,
            signature: None,
        }
    }

    /// Return the current count
    pub fn count(&self) -> u64 {
        self.counter
    }

    /// Increments the counter value.
    pub fn increment(&mut self) -> u64 {
        self.counter += 1;

        self.counter
    }

    /// Returns the next counter value,
    ///
    /// Encrypts data and updates the signature with provided sk
    pub fn update_and_sign(&mut self, unencrypted_data: Bytes, sk: &SecretKey) -> u64 {
        let next_count = self.increment();

        let pk = self.owner();

        self.encrypted_data = Bytes::from(pk.encrypt(unencrypted_data).to_bytes());

        let encrypted_data_xorname = self.encrypted_data_hash().to_vec();

        let mut bytes_to_sign = self.counter.to_be_bytes().to_vec();
        bytes_to_sign.extend(encrypted_data_xorname);

        self.signature = Some(sk.sign(&bytes_to_sign));
        next_count
    }

    /// Verifies the signature and content of the scratchpad are valid for the
    /// owner's public key.
    pub fn is_valid(&self) -> bool {
        if let Some(signature) = &self.signature {
            let mut signing_bytes = self.counter.to_be_bytes().to_vec();
            signing_bytes.extend(self.encrypted_data_hash().to_vec()); // add the count

            self.owner().verify(signature, &signing_bytes)
        } else {
            false
        }
    }

    /// Returns the encrypted_data.
    pub fn encrypted_data(&self) -> &Bytes {
        &self.encrypted_data
    }

    /// Returns the encrypted_data, decrypted via the passed SecretKey
    pub fn decrypt_data(&self, sk: &SecretKey) -> Result<Option<Bytes>> {
        Ok(sk
            .decrypt(
                &Ciphertext::from_bytes(&self.encrypted_data)
                    .map_err(|_| Error::ScratchpadCipherTextFailed)?,
            )
            .map(Bytes::from))
    }

    /// Returns the encrypted_data hash
    pub fn encrypted_data_hash(&self) -> XorName {
        XorName::from_content(&self.encrypted_data)
    }

    /// Returns the owner.
    pub fn owner(&self) -> &PublicKey {
        self.address.owner()
    }

    /// Returns the address.
    pub fn address(&self) -> &ScratchpadAddress {
        &self.address
    }

    /// Returns the NetworkAddress
    pub fn network_address(&self) -> NetworkAddress {
        NetworkAddress::ScratchpadAddress(self.address)
    }

    /// Returns the name.
    pub fn name(&self) -> XorName {
        self.address.xorname()
    }

    /// Returns size of contained encrypted_data.
    pub fn payload_size(&self) -> usize {
        self.encrypted_data.len()
    }

    /// Returns size of this scratchpad after serialisation.
    pub fn serialised_size(&self) -> usize {
        self.encrypted_data.len()
            + std::mem::size_of::<u64>()
            + std::mem::size_of::<Option<Signature>>()
    }
}
