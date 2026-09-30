use sqlx::PgPool;
use uuid::Uuid;

use crate::api_keys::{self, IssuedApiKey};

pub async fn create_with_api_key(db: &PgPool, name: &str) -> anyhow::Result<(Uuid, IssuedApiKey)> {
    let name = name.trim();
    anyhow::ensure!(
        (1..=200).contains(&name.chars().count()),
        "business name must be 1 to 200 characters"
    );

    let mut tx = db.begin().await?;
    let business_id = Uuid::now_v7();
    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, $2)")
        .bind(business_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    let key = api_keys::issue(&mut *tx, business_id).await?;
    tx.commit().await?;

    Ok((business_id, key))
}
