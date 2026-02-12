use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
    #[error("missing auth")]
    MissingAuth,
    #[error("invalid bearer token")]
    InvalidBearer,
    #[error("bearer token not configured")]
    BearerNotConfigured,
}

#[derive(Debug, Deserialize)]
struct PresignPayload {
    #[serde(rename = "v")]
    version: u8,
    #[serde(rename = "exp")]
    expiry: i64,
    #[serde(rename = "m")]
    method: String,
    #[serde(rename = "b")]
    bucket_id: String,
    #[serde(rename = "p")]
    path: String,
}

#[derive(Clone)]
pub struct AuthState {
    verifying_key: VerifyingKey,
    bearer_token: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub enum AuthMethod {
    Bearer,
    Presign,
}

impl AuthMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::Presign => "presign",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuthContext {
    pub method: AuthMethod,
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
            bearer_token: config.bearer_token.clone(),
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

        if payload.version != 1 {
            return Err(AuthError::UnsupportedVersion);
        }
        if payload.expiry < OffsetDateTime::now_utc().unix_timestamp() {
            return Err(AuthError::Expired);
        }
        if payload.method.to_uppercase() != method.to_uppercase() {
            return Err(AuthError::MethodMismatch);
        }
        if payload.bucket_id != bucket_id {
            return Err(AuthError::BucketMismatch);
        }
        if payload.path != path {
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

    pub fn verify_bearer(&self, token: &str) -> Result<(), AuthError> {
        let expected = self
            .bearer_token
            .as_deref()
            .ok_or(AuthError::BearerNotConfigured)?;
        if token == expected {
            Ok(())
        } else {
            Err(AuthError::InvalidBearer)
        }
    }
}

fn decode_key(input: &str) -> Result<Vec<u8>, AuthError> {
    URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| AuthError::InvalidKeyMaterial)
}
