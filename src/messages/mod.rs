// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod accumulating_message;
mod hash;
mod src_authority;
mod variant;

pub use self::{
    accumulating_message::{AccumulatingMessage, PlainMessage},
    hash::MessageHash,
    src_authority::SrcAuthority,
    variant::{BootstrapResponse, JoinRequest, Variant},
};
use crate::{
    error::{Result, RoutingError},
    id::FullId,
    location::DstLocation,
};

use bytes::Bytes;
use itertools::Itertools;
use std::{
    fmt::{self, Debug, Formatter},
    net::SocketAddr,
};
use xor_name::{Prefix, XorName};

/// Message sent over the network.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Message {
    /// Destination location.
    dst: DstLocation,
    /// Source authority.
    /// Messages do not need to sign this field as it is all verifiable (i.e. if the sig validates
    /// agains the public key and we know the pub key then we are good. If the proof is not recognised we
    /// ask for a longer chain that can be recognised). Therefor we don't need to sign this field.
    src: SrcAuthority,
    /// The body of the message.
    variant: Variant,
    /// Source's knowledge of the destination section key. If present, the destination can use it
    /// to determine the length of the proof of messages sent to the source so the source would
    /// trust it (the proof needs to start at this key).
    dst_key: Option<bls::PublicKey>,

    /// Serialised message, this is a signed and fully serialised message ready to send.
    #[serde(skip)]
    serialized: Bytes,
    #[serde(skip)]
    hash: MessageHash,
}

impl Message {
    /// Deserialize the message. Only called on message receipt.
    pub(crate) fn from_bytes(bytes: &Bytes) -> Result<Self> {
        let mut msg: Message = bincode::deserialize(&bytes[..])?;
        let signed_bytes = serialize_for_signing(&msg.dst, msg.dst_key.as_ref(), &msg.variant)?;
        match msg.src.clone() {
            SrcAuthority::Node {
                public_id,
                signature,
            } => {
                if public_id.verify(&signed_bytes, &signature) {
                    msg.serialized = bytes.clone();
                    msg.hash = MessageHash::from_bytes(bytes);
                    Ok(msg)
                } else {
                    Err(RoutingError::FailedSignature)
                }
            }
            SrcAuthority::Section {
                signature, proof, ..
            } => {
                // FIXME Assumes the nodes proof last key is the one signing this message
                if proof.last_key().verify(&signature, &signed_bytes) {
                    msg.serialized = bytes.clone();
                    msg.hash = MessageHash::from_bytes(bytes);
                    Ok(msg)
                } else {
                    Err(RoutingError::FailedSignature)
                }
            }
        }
    }

    /// send across wire
    pub(crate) fn to_bytes(&self) -> Bytes {
        self.serialized.clone()
    }

    /// Creates a signed message where signature is assumed valid.
    fn new_signed(
        src: SrcAuthority,
        dst: DstLocation,
        dst_key: Option<bls::PublicKey>,
        variant: Variant,
    ) -> Result<Message> {
        let mut msg = Message {
            dst,
            src,
            variant,
            dst_key,
            serialized: Default::default(),
            hash: Default::default(),
        };
        let bytes: Bytes = bincode::serialize(&msg)?.into();
        msg.serialized = bytes.clone();
        msg.hash = MessageHash::from_bytes(&bytes);
        Ok(msg)
    }

    /// Creates a signed message from single node.
    pub(crate) fn single_src(
        src: &FullId,
        dst: DstLocation,
        dst_key: Option<bls::PublicKey>,
        variant: Variant,
    ) -> Result<Self> {
        let serialized = serialize_for_signing(&dst, dst_key.as_ref(), &variant)?;
        let signature = src.sign(&serialized);
        let src = SrcAuthority::Node {
            public_id: *src.public_id(),
            signature,
        };

        Self::new_signed(src, dst, dst_key, variant)
    }

    /// Verify this message is properly signed and trusted.
    pub(crate) fn verify<'a, I>(&'a self, their_keys: I) -> Result<VerifyStatus>
    where
        I: IntoIterator<Item = (&'a Prefix<XorName>, &'a bls::PublicKey)>,
    {
        self.src
            .verify(&self.dst, self.dst_key.as_ref(), &self.variant, their_keys)
    }

    pub(crate) fn into_queued(self, sender: Option<SocketAddr>) -> QueuedMessage {
        QueuedMessage {
            message: self,
            sender,
        }
    }

    /// Getter
    pub fn dst(&self) -> &DstLocation {
        &self.dst
    }

    /// Getter
    pub fn variant(&self) -> &Variant {
        &self.variant
    }

    /// Getter
    pub fn src(&self) -> &SrcAuthority {
        &self.src
    }

    /// Getter
    pub fn dst_key(&self) -> &Option<bls::PublicKey> {
        &self.dst_key
    }
    /// Getter
    pub fn hash(&self) -> &MessageHash {
        &self.hash
    }
}

impl Debug for Message {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter
            .debug_struct("Message")
            .field("src", &self.src.src_location())
            .field("dst", &self.dst)
            .field("variant", &self.variant)
            .finish()
    }
}

#[derive(Eq, PartialEq, Debug)]
pub enum VerifyStatus {
    // The message has been fully verified.
    Full,
    // The message trust and integrity cannot be verified because it's proof is not trusted by us,
    // even though it is valid. The message should be relayed to other nodes who might be able to
    // verify it.
    Unknown,
}

impl VerifyStatus {
    pub fn require_full(self) -> Result<(), RoutingError> {
        match self {
            Self::Full => Ok(()),
            Self::Unknown => Err(RoutingError::UntrustedMessage),
        }
    }
}

pub struct QueuedMessage {
    pub message: Message,
    pub sender: Option<SocketAddr>,
}

pub fn log_verify_failure<'a, T, I>(msg: &T, error: &RoutingError, their_keys: I)
where
    T: Debug,
    I: IntoIterator<Item = (&'a Prefix<XorName>, &'a bls::PublicKey)>,
{
    log_or_panic!(
        log::Level::Error,
        "Verification failed: {:?} - {:?} --- [{:?}]",
        msg,
        error,
        their_keys.into_iter().format(", ")
    )
}

/// Status of an incomming message.
pub enum MessageStatus {
    /// Message is useful and should be handled.
    Useful,
    /// Message is useless and should be discarded.
    Useless,
    /// Message trust can't be established.
    Untrusted,
    /// We don't know how to handle the message because we are not in the right state (e.g. it
    /// needs elder but we are not)
    Unknown,
}

fn serialize_for_signing(
    dst: &DstLocation,
    dst_key: Option<&bls::PublicKey>,
    variant: &Variant,
) -> Result<Vec<u8>> {
    Ok(bincode::serialize(&(dst, dst_key, variant))?)
}
