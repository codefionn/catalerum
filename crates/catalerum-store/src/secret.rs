//! Workspace-scoped encrypted secret store (SOUL §13/§29).
//!
//! External-provider credentials (currently: external Postgres connection
//! passwords) are **encrypted at rest** with AES-256-GCM and stored in the
//! `secret_store` table, referenced from a [`Connection`](catalerum_core::model::Connection)
//! by its opaque `credential_ref`. The 32-byte master key lives only in
//! configuration/environment (`[secrets] master_key`), never in the database —
//! so a database dump alone never reveals a credential. Each row carries its own
//! random 96-bit GCM nonce; the sealed bytes include the authentication tag, so
//! tampering is detected on decrypt.

use crate::DbPool as PgPool;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use uuid::Uuid;

use catalerum_core::id::WorkspaceId;

use crate::error::{Result, StoreError};

/// Required length in bytes of the AES-256 master key.
pub const MASTER_KEY_LEN: usize = 32;

/// AES-256-GCM cipher bound to a fixed 32-byte master key. Sealing generates a
/// fresh random nonce per call and returns it alongside the ciphertext for
/// storage; opening authenticates the tag (a wrong key or tampered bytes fail).
struct Cipher {
    key: LessSafeKey,
    rng: SystemRandom,
}

impl Cipher {
    fn new(master_key: &[u8]) -> Result<Self> {
        let unbound = UnboundKey::new(&AES_256_GCM, master_key).map_err(|_| {
            StoreError::Crypto(format!(
                "master key must be {MASTER_KEY_LEN} bytes for AES-256-GCM"
            ))
        })?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
            rng: SystemRandom::new(),
        })
    }

    /// Encrypt `plaintext`; returns `(nonce, ciphertext_with_tag)`.
    fn seal(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce_bytes)
            .map_err(|_| StoreError::Crypto("secure RNG failure".into()))?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| StoreError::Crypto("encryption failed".into()))?;
        Ok((nonce_bytes.to_vec(), in_out))
    }

    /// Decrypt a `(nonce, ciphertext_with_tag)` pair. Fails closed on a wrong key
    /// or tampered ciphertext (the GCM tag no longer authenticates).
    fn open(&self, nonce_bytes: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes)
            .map_err(|_| StoreError::Crypto("malformed nonce".into()))?;
        let mut in_out = ciphertext.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| StoreError::Crypto("decryption failed (wrong key or tampered)".into()))?;
        Ok(plain.to_vec())
    }
}

/// Encrypted, workspace-scoped secret store backed by the `secret_store` table.
///
/// Construct once from the pool and the decoded master key and share it (it is
/// cheap to clone-by-`Arc` at the call site). Every operation is scoped by
/// `workspace_id`, so a `ref` minted in one workspace is invisible to another.
pub struct SecretStore {
    pool: PgPool,
    cipher: Cipher,
}

impl SecretStore {
    /// Build a secret store over `pool` using `master_key` (must be
    /// [`MASTER_KEY_LEN`] bytes). Fails if the key length is wrong.
    pub fn new(pool: PgPool, master_key: &[u8]) -> Result<Self> {
        Ok(Self {
            pool,
            cipher: Cipher::new(master_key)?,
        })
    }

    /// Encrypt `plaintext` for `workspace_id` and store it, returning the opaque
    /// `ref` to persist as a connection's `credential_ref`.
    pub async fn put(&self, workspace_id: WorkspaceId, plaintext: &[u8]) -> Result<String> {
        let (nonce, ciphertext) = self.cipher.seal(plaintext)?;
        let id = Uuid::new_v4();
        let reference = format!("sec-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO secret_store (id, workspace_id, ref, nonce, ciphertext)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(&reference)
        .bind(&nonce)
        .bind(&ciphertext)
        .execute(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(reference)
    }

    /// Encrypt `plaintext` behind a caller-chosen opaque reference, inserting or
    /// replacing it atomically. This is used for deterministic config-managed
    /// credentials so concurrent API pods converge on one secret instead of each
    /// leaking a different generated reference during first-use reconciliation.
    pub async fn put_at(
        &self,
        workspace_id: WorkspaceId,
        reference: &str,
        plaintext: &[u8],
    ) -> Result<()> {
        let (nonce, ciphertext) = self.cipher.seal(plaintext)?;
        sqlx::query(
            "INSERT INTO secret_store (id, workspace_id, ref, nonce, ciphertext)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (workspace_id, ref) DO UPDATE SET
                 nonce = EXCLUDED.nonce,
                 ciphertext = EXCLUDED.ciphertext,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(Uuid::new_v4())
        .bind(workspace_id.into_uuid())
        .bind(reference)
        .bind(&nonce)
        .bind(&ciphertext)
        .execute(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;
        Ok(())
    }

    /// Fetch and decrypt the secret named by `reference`, scoped to its
    /// workspace. Returns [`StoreError::NotFound`] if absent.
    pub async fn get(&self, workspace_id: WorkspaceId, reference: &str) -> Result<Vec<u8>> {
        let row: (Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT nonce, ciphertext FROM secret_store
             WHERE workspace_id = $1 AND ref = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(reference)
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;
        self.cipher.open(&row.0, &row.1)
    }

    /// Re-encrypt the secret behind an existing `reference` in place (rotation /
    /// credential update). No-op error if the reference does not exist.
    pub async fn replace(
        &self,
        workspace_id: WorkspaceId,
        reference: &str,
        plaintext: &[u8],
    ) -> Result<()> {
        let (nonce, ciphertext) = self.cipher.seal(plaintext)?;
        let done = sqlx::query(
            "UPDATE secret_store SET nonce = $3, ciphertext = $4, updated_at = now()
             WHERE workspace_id = $1 AND ref = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(reference)
        .bind(&nonce)
        .bind(&ciphertext)
        .execute(&self.pool)
        .await
        .map_err(StoreError::from_sqlx)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Delete the secret named by `reference` (idempotent).
    pub async fn delete(&self, workspace_id: WorkspaceId, reference: &str) -> Result<()> {
        sqlx::query("DELETE FROM secret_store WHERE workspace_id = $1 AND ref = $2")
            .bind(workspace_id.into_uuid())
            .bind(reference)
            .execute(&self.pool)
            .await
            .map_err(StoreError::from_sqlx)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; MASTER_KEY_LEN] {
        // A fixed, obviously-non-secret test key.
        let mut k = [0u8; MASTER_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn seal_open_round_trips() {
        let cipher = Cipher::new(&key()).unwrap();
        let (nonce, ct) = cipher.seal(b"hunter2").unwrap();
        // Ciphertext is not the plaintext, and carries the 16-byte GCM tag.
        assert_ne!(ct, b"hunter2");
        assert_eq!(ct.len(), b"hunter2".len() + 16);
        assert_eq!(cipher.open(&nonce, &ct).unwrap(), b"hunter2");
    }

    #[test]
    fn tampered_ciphertext_fails_closed() {
        let cipher = Cipher::new(&key()).unwrap();
        let (nonce, mut ct) = cipher.seal(b"secret").unwrap();
        ct[0] ^= 0xff; // flip a bit
        assert!(cipher.open(&nonce, &ct).is_err());
    }

    #[test]
    fn wrong_key_fails_closed() {
        let a = Cipher::new(&key()).unwrap();
        let (nonce, ct) = a.seal(b"secret").unwrap();
        let mut other = key();
        other[0] ^= 0xff;
        let b = Cipher::new(&other).unwrap();
        assert!(b.open(&nonce, &ct).is_err());
    }

    #[test]
    fn wrong_key_length_is_rejected() {
        assert!(Cipher::new(&[0u8; 16]).is_err());
    }

    #[test]
    fn nonce_is_unique_per_seal() {
        let cipher = Cipher::new(&key()).unwrap();
        let (n1, _) = cipher.seal(b"x").unwrap();
        let (n2, _) = cipher.seal(b"x").unwrap();
        assert_ne!(n1, n2, "each seal must use a fresh random nonce");
    }
}
