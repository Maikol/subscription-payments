use anyhow::{anyhow, ensure, Context};
use base64::{prelude::BASE64_URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, NaiveDateTime, Utc};
use ethers::{
    abi::Address, contract::abigen, prelude::k256::ecdsa::SigningKey, signers::Wallet,
    types::Signature, utils::hash_message,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{serde_as, skip_serializing_none, FromInto};
use std::{
    io::{self, Write as _},
    str::FromStr as _,
};

pub mod subscription_tier;

abigen!(
    Subscriptions,
    "../contracts/build/Subscriptions.abi",
    event_derives(serde::Deserialize, serde::Serialize);
    IERC20,
    "../contracts/build/IERC20.abi",
    event_derives(serde::Deserialize, serde::Serialize);
);

// This is necessary intermediary to get the Address wrapper type over bytes to serialize &
// deserialize as bytes in the CBOR representation.
// See https://github.com/jonasbb/serde_with/discussions/557
#[derive(Clone, Debug)]
struct AddressBytes([u8; 20]);
#[rustfmt::skip]
impl From<AddressBytes> for Address { fn from(value: AddressBytes) -> Self { value.0.into() } }
#[rustfmt::skip]
impl From<Address> for AddressBytes { fn from(value: Address) -> Self { Self(value.0) } }
impl Serialize for AddressBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&format!("{:?}", Address::from(self.0)))
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}
impl<'de> Deserialize<'de> for AddressBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            Address::from_str(&String::deserialize(deserializer)?)
                .map(|address| Self(address.0))
                .map_err(serde::de::Error::custom)
        } else {
            // There should be a better way, but this works.
            #[serde_as]
            #[derive(Deserialize)]
            #[serde(transparent)]
            struct AsBytes(#[serde_as(as = "serde_with::Bytes")] [u8; 20]);
            AsBytes::deserialize(deserializer).map(|bytes| Self(bytes.0))
        }
    }
}

#[serde_as]
#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TicketPayload {
    /// EIP-155 ID for the chain on which the contract is deployed.
    pub chain_id: u64,
    /// Address of the subscriptions contract.
    #[serde_as(as = "FromInto<AddressBytes>")]
    pub contract: Address,
    /// Address associated with the secret key used to sign the ticket.
    #[serde_as(as = "FromInto<AddressBytes>")]
    pub signer: Address,
    /// Required to when the authorized `signer` is not the `user` associated with a subscription.
    /// When omitted, the `signer` is implied to be equal to the `user`.
    #[serde_as(as = "Option<FromInto<AddressBytes>>")]
    pub user: Option<Address>,
    /// Optional nice name.
    pub name: Option<String>,
    // /// Unique identifier, used in conjunction with additional options such as `max_uses`.
    // pub id: u64,
    // /// Maximum uses for tickets with matching identifiers. Defaults to 1 when omitted.
    // pub max_uses: Option<u64>,
    // /// Unix timestamp after which the ticket is invalid.
    // pub expiration: Option<u64>,
    /// Comma-separated list of subgraphs that can be queried with this ticket.
    pub allowed_subgraphs: Option<String>,
    /// Comma-separated list of subgraph deployments that can be queried with this ticket.
    pub allowed_deployments: Option<String>,
    /// Comma-separated list of origin domains that can send queries with this ticket.
    pub allowed_domains: Option<String>,
}

impl TicketPayload {
    pub fn user(&self) -> Address {
        self.user.unwrap_or(self.signer)
    }

    pub fn from_ticket_base64(ticket: &str) -> anyhow::Result<(Self, Signature)> {
        let ticket = base64::prelude::BASE64_URL_SAFE_NO_PAD
            .decode(ticket)
            .context("invalid base64 (URL, nopad)")?;

        let signature_start = ticket.len().saturating_sub(65);
        let signature = ticket[signature_start..]
            .try_into()
            .map(Signature::from)
            .context("invalid signature")?;

        let payload: TicketPayload =
            serde_cbor_2::de::from_reader(&ticket[..signature_start]).context("invalid payload")?;
        payload
            .verify(&signature)
            .context("failed to recover signer")?;
        Ok((payload, signature))
    }

    pub fn to_ticket_base64(&self, wallet: &Wallet<SigningKey>) -> anyhow::Result<String> {
        let ticket = self.encode(wallet)?;
        Ok(BASE64_URL_SAFE_NO_PAD.encode(ticket))
    }

    pub fn encode(&self, wallet: &Wallet<SigningKey>) -> anyhow::Result<Vec<u8>> {
        let mut buf = serde_cbor_2::ser::to_vec(self)?;
        buf.append(&mut self.sign_hash(wallet)?.to_vec());
        Ok(buf)
    }

    pub fn sign_hash(&self, wallet: &Wallet<SigningKey>) -> anyhow::Result<Signature> {
        let hash = hash_message(self.verification_message());
        Ok(wallet.sign_hash(hash)?)
    }

    pub fn verify(&self, signature: &Signature) -> anyhow::Result<Address> {
        let hash = hash_message(self.verification_message());
        let recovered_signer = signature.recover(hash)?;
        ensure!(
            recovered_signer == self.signer,
            "recovered signer does not match claim"
        );
        Ok(self.signer)
    }

    pub fn verification_message(&self) -> String {
        let mut cursor: io::Cursor<Vec<u8>> = io::Cursor::default();
        if let Some(allowed_deployments) = &self.allowed_deployments {
            writeln!(&mut cursor, "allowed_deployments: {}", allowed_deployments).unwrap();
        }
        if let Some(allowed_domains) = &self.allowed_domains {
            writeln!(&mut cursor, "allowed_domains: {}", allowed_domains).unwrap();
        }
        if let Some(allowed_subgraphs) = &self.allowed_subgraphs {
            writeln!(&mut cursor, "allowed_subgraphs: {}", allowed_subgraphs).unwrap();
        }
        writeln!(&mut cursor, "chain_id: {}", self.chain_id).unwrap();
        writeln!(&mut cursor, "contract: {:?}", self.contract).unwrap();
        if let Some(name) = &self.name {
            writeln!(&mut cursor, "name: {}", name).unwrap();
        }
        writeln!(&mut cursor, "signer: {:?}", self.signer).unwrap();
        if let Some(user) = self.user {
            writeln!(&mut cursor, "user: {:?}", user).unwrap();
        }
        unsafe { String::from_utf8_unchecked(cursor.into_inner()) }
    }
}

#[derive(Debug)]
pub struct Subscription {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub rate: u128,
}

impl TryFrom<(u64, u64, u128)> for Subscription {
    type Error = anyhow::Error;
    fn try_from(from: (u64, u64, u128)) -> Result<Self, Self::Error> {
        let (start, end, rate) = from;
        let to_datetime = |t: u64| {
            NaiveDateTime::from_timestamp_opt(t.try_into()?, 0)
                .ok_or_else(|| anyhow!("invalid timestamp"))
                .map(|t| DateTime::<Utc>::from_naive_utc_and_offset(t, Utc))
        };
        let start = to_datetime(start)?;
        let end = to_datetime(end)?;
        Ok(Self { start, end, rate })
    }
}

#[cfg(test)]
#[test]
fn test_ticket() {
    use ethers::signers::Signer as _;

    let wallet =
        Wallet::from_str("0x4f3edf983ac636a65a842ce7c78d9aa706d3b113bce9c46f30d7d21715b23b1d")
            .unwrap()
            .with_chain_id(1337_u64);

    let payload = TicketPayload {
        chain_id: wallet.chain_id(),
        contract: "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"
            .parse()
            .unwrap(),
        signer: wallet.address(),
        user: None,
        name: None,
        allowed_subgraphs: None,
        allowed_deployments: None,
        allowed_domains: None,
    };
    println!("{payload:#?}");
    let ticket = payload.to_ticket_base64(&wallet).unwrap();
    println!("ticket: {ticket}");
    let (extracted_payload, signature) = TicketPayload::from_ticket_base64(&ticket).unwrap();
    println!("signature: {}", hex::encode(signature.to_vec()));
    assert_eq!(payload, extracted_payload);
}
