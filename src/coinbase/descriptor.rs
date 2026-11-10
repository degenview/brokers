//! What the interface needs to know about Coinbase without naming it anywhere else.

use crate::adapter::{AccountModel, ClientIdRules, CredentialField, Descriptor, FieldKind};
use crate::model::types::BrokerId;

pub const ID: BrokerId = BrokerId::new("coinbase");

pub const FIELD_NAME: &str = "name";
pub const FIELD_PRIVATE_KEY: &str = "private_key";

const FIELDS: &[CredentialField] = &[
    CredentialField {
        key: FIELD_NAME,
        label: "API key name",
        hint: "organizations/…/apiKeys/…",
        secret: false,
        kind: FieldKind::Text,
    },
    CredentialField {
        key: FIELD_PRIVATE_KEY,
        label: "Private key",
        hint: "-----BEGIN EC PRIVATE KEY----- … (or a base64 Ed25519 key)",
        secret: true,
        kind: FieldKind::Pem,
    },
];

pub static DESCRIPTOR: Descriptor = Descriptor {
    id: ID,
    display_name: "Coinbase",
    credential_fields: FIELDS,
    setup_steps: &[
        "Open the Coinbase Developer Platform (portal.cdp.coinbase.com) and sign in.",
        "Go to API keys and create a Secret API key for the portfolio you want to trade.",
        "Choose the ECDSA signature algorithm if you are offered one.",
        "Tick View, and Trade only if you want to place orders from DegenView.",
        "Do not tick Transfer. DegenView never moves funds and does not need it.",
        "Copy the key name and the private key (shown once) into the fields below.",
    ],
    permission_note: "View lets DegenView read balances and orders. Trade lets it place and cancel orders. Transfer is never needed.",
    docs_url: "https://docs.cdp.coinbase.com/coinbase-app/authentication-authorization/api-key-authentication",
    client_id: ClientIdRules {
        max_len: 128,
        // "If the ID provided is not unique, the order will not be created and the order
        // corresponding with that ID will be returned instead." (create-order reference)
        idempotent: true,
    },
    market_source: "coinbase",
    account_model: AccountModel::Portfolios,
    // Coinbase's private-endpoint limits are not documented; stay well under any plausible one.
    min_request_gap_ms: 120,
};
