use super::*;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use hkdf::Hkdf;
use object_store::{
    ClientConfigKey, ObjectStoreExt, RetryConfig,
    aws::{AmazonS3, AmazonS3Builder, AmazonS3ConfigKey},
    path::Path as ObjectPath,
};
use sha2::Sha256;
use zeroize::Zeroizing;

const V2_PREFIX: &[u8; 4] = b"VC2\x01";

pub(super) struct Storage {
    bucket: AmazonS3,
    /// Legacy raw-key cipher kept solely for reading pre-VC2 objects.
    legacy_cipher: Aes256Gcm,
    /// Domain-separated master key for HKDF per-AAD AEAD subkey derivation.
    aead_master: Zeroizing<[u8; 32]>,
    /// Legacy raw key for backwards-compatible HMAC fingerprints (zeroized on drop).
    fingerprint_key: Zeroizing<Vec<u8>>,
}

impl Storage {
    pub fn new() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let raw_key = require_env("VENTICODE_CONTENT_KEY")?;
        let key = Zeroizing::new(
            STANDARD
                .decode(raw_key.trim())
                .map_err(|e| format!("Invalid Base64 in VENTICODE_CONTENT_KEY: {e}"))?,
        );
        let legacy_cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| "Expected a 256-bit Venticode key")?;
        let aead_master = derive_master_key(&key, b"venticode-aead-master-v2");
        let bucket_name = require_env("S3_BUCKET")?;
        let retry_config = RetryConfig {
            max_retries: 3,
            retry_timeout: Duration::from_secs(30),
            ..RetryConfig::default()
        };
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(bucket_name)
            .with_config(
                AmazonS3ConfigKey::Client(ClientConfigKey::ConnectTimeout),
                "10s",
            )
            .with_config(AmazonS3ConfigKey::Client(ClientConfigKey::Timeout), "30s")
            .with_retry(retry_config);
        if let Ok(endpoint) = env::var("S3_ENDPOINT")
            && !endpoint.trim().is_empty()
        {
            builder = builder.with_endpoint(endpoint.trim());
        }
        Ok(Self {
            bucket: builder.build()?,
            legacy_cipher,
            aead_master,
            fingerprint_key: key,
        })
    }

    pub fn fingerprint(&self, content: &str) -> String {
        use hmac::{Hmac, Mac};
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(&self.fingerprint_key).expect("fixed-size key");
        mac.update(b"venticode-message-id-v1");
        mac.update(content.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn path(id: Uuid) -> ObjectPath {
        ObjectPath::from(format!("venticode/events/{id}"))
    }

    fn object_cipher(&self, aad: &[u8]) -> Aes256Gcm {
        let mut subkey = Zeroizing::new([0u8; 32]);
        let hk = Hkdf::<Sha256>::new(Some(aad), &*self.aead_master);
        hk.expand(b"venticode-object-aes256gcm-v2", &mut *subkey)
            .expect("valid HKDF length");
        Aes256Gcm::new_from_slice(&*subkey).expect("256-bit subkey")
    }

    pub fn seal(&self, aad: &[u8], data: &Value) -> Result<Vec<u8>> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let raw = serde_json::to_vec(data).map_err(|_| bad())?;
        let cipher = self.object_cipher(aad);
        let ciphertext = cipher
            .encrypt(&nonce, Payload { msg: &raw, aad })
            .map_err(|_| unavailable())?;
        let mut body = Vec::with_capacity(V2_PREFIX.len() + nonce.len() + ciphertext.len());
        body.extend_from_slice(V2_PREFIX);
        body.extend_from_slice(&nonce);
        body.extend_from_slice(&ciphertext);
        Ok(body)
    }

    pub fn open(&self, aad: &[u8], bytes: &[u8]) -> Result<Value> {
        let plain = if let Some(rest) = bytes.strip_prefix(V2_PREFIX) {
            if rest.len() < 28 {
                return Err(unavailable());
            }
            let cipher = self.object_cipher(aad);
            cipher
                .decrypt(
                    Nonce::from_slice(&rest[..12]),
                    Payload {
                        msg: &rest[12..],
                        aad,
                    },
                )
                .map_err(|_| unavailable())?
        } else {
            if bytes.len() < 28 {
                return Err(unavailable());
            }
            self.legacy_cipher
                .decrypt(
                    Nonce::from_slice(&bytes[..12]),
                    Payload {
                        msg: &bytes[12..],
                        aad,
                    },
                )
                .map_err(|_| unavailable())?
        };
        serde_json::from_slice(&plain).map_err(|_| unavailable())
    }

    pub async fn put(&self, id: Uuid, data: &Value) -> Result<()> {
        let body = self.seal(id.as_bytes(), data)?;
        self.bucket
            .put(&Self::path(id), body.into())
            .await
            .map_err(|error| {
                tracing::error!(object_id = %id, error = %error, "S3 put failed");
                unavailable()
            })?;
        Ok(())
    }

    pub async fn get(&self, id: Uuid) -> Result<Value> {
        self.get_optional(id).await?.ok_or_else(unavailable)
    }

    pub async fn get_optional(&self, id: Uuid) -> Result<Option<Value>> {
        let response = match self.bucket.get(&Self::path(id)).await {
            Ok(res) => res,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => {
                tracing::error!(object_id = %id, error = %error, "S3 get failed");
                return Err(unavailable());
            }
        };
        let bytes = response.bytes().await.map_err(|error| {
            tracing::error!(object_id = %id, error = %error, "S3 read bytes failed");
            unavailable()
        })?;
        self.open(id.as_bytes(), &bytes)
            .inspect_err(|_| {
                tracing::error!(object_id = %id, "failed to decrypt/parse stored object");
            })
            .map(Some)
    }
}

fn derive_master_key(raw: &[u8], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    let hk = Hkdf::<Sha256>::new(None, raw);
    hk.expand(info, &mut *out).expect("valid HKDF length");
    out
}

pub(super) async fn maintain(app: App) {
    let mut shutdown = app.shutdown.clone();
    loop {
        if *shutdown.borrow() {
            break;
        }
        if let Err(error) = sweep(&app).await {
            tracing::warn!(status = error.0.as_u16(), message = %error.1, "storage maintenance sweep failed, will retry");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = shutdown.changed() => break,
        }
    }
}

async fn sweep(app: &App) -> Result<()> {
    if app.standalone.is_some()
        && let Err(error) = sqlx::query(
            "DELETE FROM standalone_sessions WHERE expires_at<=now() OR absolute_expires_at<=now()",
        )
        .execute(&app.db)
        .await
    {
        tracing::warn!(error = %error, "failed to purge expired standalone sessions");
    }
    match sqlx::query(
        "SELECT id,active_run FROM conversations WHERE lease_until<now() AND active_run IS NOT NULL LIMIT 50",
    )
    .fetch_all(&app.db)
    .await
    {
        Ok(expired) => {
            for row in expired {
                let id: Uuid = row.get("id");
                let run: Uuid = row.get("active_run");
                if let Err(error) = runs::finish(app, id, run, runs::Completion::Expired).await
                    && error.0 != StatusCode::CONFLICT
                {
                    tracing::warn!(conversation_id = %id, run_id = %run, status = error.0.as_u16(), "failed to expire stale run");
                }
            }
        }
        Err(error) => tracing::warn!(error = %error, "failed to query expired runs"),
    }
    if let Err(error) = sqlx::query(
        "UPDATE mentions SET state='uncertain',lease_until=NULL WHERE state='sending' AND lease_until<now()",
    )
    .execute(&app.db)
    .await
    {
        tracing::warn!(error = %error, "failed to recover stale sending mentions");
    }
    if let Err(error) = sqlx::query("DELETE FROM link_challenges WHERE expires_at<=now()")
        .execute(&app.db)
        .await
    {
        tracing::warn!(error = %error, "failed to purge expired link challenges");
    }
    if let Err(error) =
        sqlx::query("DELETE FROM link_notices WHERE created_at < now() - interval '7 days'")
            .execute(&app.db)
            .await
    {
        tracing::warn!(error = %error, "failed to purge old link notices");
    }
    // Claim batches in short transactions and perform S3 deletes concurrently outside any DB lock.
    const BATCH_SIZE: i64 = 64;
    for _ in 0..16 {
        if *app.shutdown.borrow() {
            break;
        }
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "WITH claimed AS (
                SELECT object_id FROM object_cleanup
                WHERE due_at <= now()
                ORDER BY due_at
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE object_cleanup c
            SET due_at = now() + interval '2 minutes'
            FROM claimed
            WHERE c.object_id = claimed.object_id
            RETURNING c.object_id",
        )
        .bind(BATCH_SIZE)
        .fetch_all(&app.db)
        .await?;
        if ids.is_empty() {
            break;
        }
        let count = ids.len();
        let results: Vec<(Uuid, bool)> = futures_util::stream::iter(ids)
            .map(|id| {
                let storage = app.storage.clone();
                async move {
                    match storage.bucket.delete(&Storage::path(id)).await {
                        Ok(()) | Err(object_store::Error::NotFound { .. }) => (id, true),
                        Err(error) => {
                            tracing::warn!(object_id = %id, error = %error, "S3 delete failed during GC, deferring");
                            (id, false)
                        }
                    }
                }
            })
            .buffer_unordered(16)
            .collect()
            .await;
        let deleted: Vec<Uuid> = results
            .into_iter()
            .filter_map(|(id, ok)| ok.then_some(id))
            .collect();
        if !deleted.is_empty() {
            let mut tx = app.db.begin().await?;
            sqlx::query("DELETE FROM object_cleanup WHERE object_id = ANY($1)")
                .bind(&deleted)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM stored_objects WHERE object_id = ANY($1)")
                .bind(&deleted)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        if (count as i64) < BATCH_SIZE {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_storage(raw_key: &[u8]) -> Storage {
        let bucket = AmazonS3Builder::new()
            .with_bucket_name("test-bucket")
            .with_region("us-east-1")
            .with_access_key_id("test")
            .with_secret_access_key("test")
            .build()
            .unwrap();
        Storage {
            bucket,
            legacy_cipher: Aes256Gcm::new_from_slice(raw_key).unwrap(),
            aead_master: derive_master_key(raw_key, b"venticode-aead-master-v2"),
            fingerprint_key: Zeroizing::new(raw_key.to_vec()),
        }
    }

    #[test]
    fn vc2_seal_open_uses_hkdf_and_reads_legacy_v1_objects() {
        let storage = test_storage(&[7u8; 32]);
        let aad = Uuid::new_v4();
        let payload = json!({"type": "text", "delta": "ciao"});
        let sealed = storage.seal(aad.as_bytes(), &payload).unwrap();
        assert!(sealed.starts_with(V2_PREFIX));
        assert_eq!(storage.open(aad.as_bytes(), &sealed).unwrap(), payload);
        // Wrong AAD must fail decryption.
        assert!(storage.open(Uuid::new_v4().as_bytes(), &sealed).is_err());

        // Legacy v1 ciphertext encrypted directly with raw_key still opens cleanly.
        let nonce = Nonce::from_slice(&[3u8; 12]);
        let mut legacy = nonce.to_vec();
        legacy.extend(
            storage
                .legacy_cipher
                .encrypt(
                    nonce,
                    Payload {
                        msg: &serde_json::to_vec(&payload).unwrap(),
                        aad: aad.as_bytes(),
                    },
                )
                .unwrap(),
        );
        assert_eq!(storage.open(aad.as_bytes(), &legacy).unwrap(), payload);
    }
}
