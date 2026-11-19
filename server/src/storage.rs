use super::*;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use object_store::{
    ObjectStoreExt,
    aws::{AmazonS3, AmazonS3Builder},
    path::Path as ObjectPath,
};
pub(super) struct Storage {
    bucket: AmazonS3,
    cipher: Aes256Gcm,
    key: Vec<u8>,
}
impl Storage {
    pub fn new() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let raw_key = require_env("VENTICODE_CONTENT_KEY")?;
        let key = STANDARD
            .decode(raw_key.trim())
            .map_err(|e| format!("Invalid Base64 in VENTICODE_CONTENT_KEY: {e}"))?;
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| "Expected a 256-bit Venticode key")?;
        let bucket_name = require_env("S3_BUCKET")?;
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket_name);
        if let Ok(endpoint) = env::var("S3_ENDPOINT")
            && !endpoint.trim().is_empty()
        {
            builder = builder.with_endpoint(endpoint.trim());
        }
        Ok(Self {
            bucket: builder.build()?,
            cipher,
            key,
        })
    }
    pub fn fingerprint(&self, content: &str) -> String {
        use hmac::{Hmac, Mac};
        let mut mac =
            <Hmac<sha2::Sha256> as Mac>::new_from_slice(&self.key).expect("fixed-size key");
        mac.update(b"venticode-message-id-v1");
        mac.update(content.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
    fn path(id: Uuid) -> ObjectPath {
        ObjectPath::from(format!("venticode/events/{id}"))
    }
    pub fn seal(&self, aad: &[u8], data: &Value) -> Result<Vec<u8>> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let raw = serde_json::to_vec(data).map_err(|_| bad())?;
        let mut body = nonce.to_vec();
        body.extend(
            self.cipher
                .encrypt(&nonce, Payload { msg: &raw, aad })
                .map_err(|_| unavailable())?,
        );
        Ok(body)
    }
    pub fn open(&self, aad: &[u8], bytes: &[u8]) -> Result<Value> {
        if bytes.len() < 28 {
            return Err(unavailable());
        }
        let plain = self
            .cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad,
                },
            )
            .map_err(|_| unavailable())?;
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
        let bytes = self
            .bucket
            .get(&Self::path(id))
            .await
            .map_err(|error| {
                tracing::error!(object_id = %id, error = %error, "S3 get failed");
                unavailable()
            })?
            .bytes()
            .await
            .map_err(|error| {
                tracing::error!(object_id = %id, error = %error, "S3 read bytes failed");
                unavailable()
            })?;
        self.open(id.as_bytes(), &bytes).inspect_err(|_| {
            tracing::error!(object_id = %id, "failed to decrypt/parse stored object");
        })
    }
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
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            _ = shutdown.changed() => break,
        }
    }
}
async fn sweep(app: &App) -> Result<()> {
    if app.standalone.is_some()
        && let Err(error) = sqlx::query("DELETE FROM standalone_sessions WHERE expires_at<=now()")
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
    for _ in 0..100 {
        let mut tx = app.db.begin().await?;
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT object_id FROM object_cleanup WHERE due_at<=now() ORDER BY due_at LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            break;
        };
        match app.storage.bucket.delete(&Storage::path(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {
                sqlx::query("DELETE FROM object_cleanup WHERE object_id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM stored_objects WHERE object_id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            Err(error) => {
                tracing::warn!(object_id = %id, error = %error, "S3 delete failed during GC, deferring");
                sqlx::query(
                    "UPDATE object_cleanup SET due_at=now()+interval '1 minute' WHERE object_id=$1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
    }
    Ok(())
}
