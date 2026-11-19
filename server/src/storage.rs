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
        let key = STANDARD.decode(env::var("VENTICODE_CONTENT_KEY")?)?;
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| "Expected a 256-bit Venticode key")?;
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(env::var("S3_BUCKET")?);
        if let Ok(endpoint) = env::var("S3_ENDPOINT") {
            builder = builder.with_endpoint(endpoint);
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
            .map_err(|_| unavailable())?;
        Ok(())
    }
    pub async fn get(&self, id: Uuid) -> Result<Value> {
        let bytes = self
            .bucket
            .get(&Self::path(id))
            .await
            .map_err(|_| unavailable())?
            .bytes()
            .await
            .map_err(|_| unavailable())?;
        self.open(id.as_bytes(), &bytes)
    }
}
pub(super) async fn maintain(app: App) {
    loop {
        if let Err(_error) = sweep(&app).await {
            eprintln!("Venticode storage maintenance will retry");
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
async fn sweep(app: &App) -> Result<()> {
    if app.standalone.is_some() {
        sqlx::query("DELETE FROM standalone_sessions WHERE expires_at<=now()")
            .execute(&app.db)
            .await?;
    }
    let expired = sqlx::query("SELECT id,active_run FROM conversations WHERE lease_until<now() AND active_run IS NOT NULL LIMIT 50").fetch_all(&app.db).await?;
    for row in expired {
        runs::finish(
            app,
            row.get("id"),
            row.get("active_run"),
            runs::Completion::Expired,
        )
        .await?;
    }
    sqlx::query("UPDATE mentions SET state='uncertain',lease_until=NULL WHERE state='sending' AND lease_until<now()").execute(&app.db).await?;
    sqlx::query("DELETE FROM link_challenges WHERE expires_at<=now()")
        .execute(&app.db)
        .await?;
    for _ in 0..50 {
        let mut tx = app.db.begin().await?;
        let id:Option<Uuid>=sqlx::query_scalar("SELECT object_id FROM object_cleanup WHERE due_at<=now() ORDER BY due_at LIMIT 1 FOR UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await?;
        let Some(id) = id else {
            break;
        };
        match app.storage.bucket.delete(&Storage::path(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(_) => return Err(unavailable()),
        }
        sqlx::query("DELETE FROM object_cleanup WHERE object_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM stored_objects WHERE object_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}
