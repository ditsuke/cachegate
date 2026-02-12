use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde::Deserialize;
use thiserror::Error;
use time::OffsetDateTime;

use crate::config::AuthConfig;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("malformed signature")]
    MalformedSignature,
    #[error("malformed payload")]
    MalformedPayload,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("unsupported version")]
    UnsupportedVersion,
    #[error("expired signature")]
    Expired,
    #[error("method mismatch")]
    MethodMismatch,
    #[error("bucket mismatch")]
    BucketMismatch,
    #[error("path mismatch")]
    PathMismatch,
    #[error("invalid key material")]
    InvalidKeyMaterial,
    #[error("public and private keys do not match")]
    KeyMismatch,
}

#[derive(Debug, Deserialize)]
struct PresignPayload {
    v: u8,
    exp: i64,
    m: String,
    b: String,
    p: String,
}

#[derive(Clone)]
pub struct AuthState {
    verifying_key: VerifyingKey,
}

impl AuthState {
    pub fn from_config(config: &AuthConfig) -> Result<Self, AuthError> {
        let public_bytes = decode_key(&config.public_key)?;
        let private_bytes = decode_key(&config.private_key)?;

        let public_key = VerifyingKey::from_bytes(
            &public_bytes
                .try_into()
                .map_err(|_| AuthError::InvalidKeyMaterial)?,
        )
        .map_err(|_| AuthError::InvalidKeyMaterial)?;

        let signing_key = SigningKey::from_bytes(
            &private_bytes
                .try_into()
                .map_err(|_| AuthError::InvalidKeyMaterial)?,
        );

        let derived = signing_key.verifying_key();
        if derived != public_key {
            return Err(AuthError::KeyMismatch);
        }

        Ok(Self {
            verifying_key: public_key,
        })
    }

    pub fn verify(
        &self,
        method: &str,
        bucket_id: &str,
        path: &str,
        sig: &str,
    ) -> Result<(), AuthError> {
        let (payload_b64, signature_b64) =
            sig.split_once('.').ok_or(AuthError::MalformedSignature)?;

        let payload_bytes = URL_SAFE_NO_PAD
            .decode(payload_b64)
            .map_err(|_| AuthError::MalformedPayload)?;
        let payload: PresignPayload =
            serde_json::from_slice(&payload_bytes).map_err(|_| AuthError::MalformedPayload)?;

        if payload.v != 1 {
            return Err(AuthError::UnsupportedVersion);
        }
        if payload.exp < OffsetDateTime::now_utc().unix_timestamp() {
            return Err(AuthError::Expired);
        }
        if payload.m.to_uppercase() != method.to_uppercase() {
            return Err(AuthError::MethodMismatch);
        }
        if payload.b != bucket_id {
            return Err(AuthError::BucketMismatch);
        }
        if payload.p != path {
            return Err(AuthError::PathMismatch);
        }

        let signature_bytes = URL_SAFE_NO_PAD
            .decode(signature_b64)
            .map_err(|_| AuthError::MalformedSignature)?;
        let signature = Signature::from_bytes(
            &signature_bytes
                .try_into()
                .map_err(|_| AuthError::MalformedSignature)?,
        );

        self.verifying_key
            .verify_strict(&payload_bytes, &signature)
            .map_err(|_| AuthError::InvalidSignature)
    }
}

fn decode_key(input: &str) -> Result<Vec<u8>, AuthError> {
    URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| AuthError::InvalidKeyMaterial)
}
