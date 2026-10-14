use chrono::{DateTime, Utc};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use std::str::FromStr;
use thiserror::Error;
use tokio_postgres::NoTls;
use uuid::Uuid;

use crate::jwt::{self, KeySet, PublicJwk, SigningKey};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    first_name TEXT NOT NULL,
    last_name TEXT NOT NULL,
    coding_data_retention_opt_out BOOLEAN NOT NULL DEFAULT FALSE,
    failed_logins INT NOT NULL DEFAULT 0,
    locked_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS browser_sessions (
    id TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS auth_transactions (
    state TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    nonce TEXT NOT NULL,
    scope TEXT NOT NULL,
    user_id UUID REFERENCES users(id) ON DELETE CASCADE,
    code_hash TEXT,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX IF NOT EXISTS auth_transactions_code_hash
    ON auth_transactions (code_hash) WHERE code_hash IS NOT NULL;
CREATE TABLE IF NOT EXISTS refresh_tokens (
    token_hash TEXT PRIMARY KEY,
    family_id UUID NOT NULL,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS refresh_tokens_family ON refresh_tokens (family_id);
CREATE TABLE IF NOT EXISTS device_grants (
    device_code_hash TEXT PRIMARY KEY,
    user_code TEXT NOT NULL UNIQUE,
    client_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    status TEXT NOT NULL,
    user_id UUID REFERENCES users(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    interval_secs INT NOT NULL,
    poll_after TIMESTAMPTZ,
    consumed_at TIMESTAMPTZ
);
CREATE TABLE IF NOT EXISTS signing_keys (
    kid TEXT PRIMARY KEY,
    pkcs8_der BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    retired_at TIMESTAMPTZ
);
CREATE TABLE IF NOT EXISTS login_attempts (
    id BIGSERIAL PRIMARY KEY,
    ip TEXT NOT NULL,
    email TEXT,
    succeeded BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS login_attempts_ip ON login_attempts (ip, created_at);
CREATE TABLE IF NOT EXISTS rate_buckets (
    key TEXT PRIMARY KEY,
    hits INT NOT NULL,
    window_start TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS remote_sessions (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    title TEXT,
    cwd TEXT,
    status TEXT,
    metadata TEXT NOT NULL DEFAULT '{}',
    messages TEXT NOT NULL DEFAULT '[]',
    agent_id TEXT,
    share_id TEXT UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, session_id)
);
CREATE TABLE IF NOT EXISTS conversations (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    starred BOOLEAN NOT NULL DEFAULT FALSE,
    workspaces TEXT NOT NULL DEFAULT '[]',
    deleted_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, conversation_id)
);
CREATE TABLE IF NOT EXISTS workspaces (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL DEFAULT '',
    kind TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, workspace_id)
);
CREATE TABLE IF NOT EXISTS sandbox_environments (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    environment_id TEXT NOT NULL,
    body TEXT NOT NULL,
    variables TEXT NOT NULL DEFAULT '[]',
    secrets TEXT NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, environment_id)
);
CREATE TABLE IF NOT EXISTS sandbox_sessions (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    sandbox_id TEXT NOT NULL,
    environment_id TEXT,
    source_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, sandbox_id)
);
CREATE TABLE IF NOT EXISTS registry_sessions (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    cwd TEXT NOT NULL,
    gcs_trace_prefix TEXT NOT NULL,
    model_id TEXT,
    repo_remote_url TEXT,
    hostname TEXT,
    repo_head_at_end TEXT,
    summary TEXT NOT NULL DEFAULT '',
    first_prompt TEXT,
    last_turn_number INT NOT NULL DEFAULT 0,
    restorable_turn_number INT,
    status TEXT NOT NULL DEFAULT 'active',
    gcs_bucket TEXT NOT NULL DEFAULT 'cortex',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_active_at TIMESTAMPTZ,
    PRIMARY KEY (user_id, session_id)
);
CREATE TABLE IF NOT EXISTS storage_objects (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    content BYTEA NOT NULL,
    content_type TEXT NOT NULL,
    generation BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, path)
);
CREATE TABLE IF NOT EXISTS storage_grants (
    token_hash TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    op TEXT NOT NULL,
    content_type TEXT,
    upload_id TEXT,
    part_number INT,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS multipart_uploads (
    upload_id TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    total_size BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS multipart_parts (
    upload_id TEXT NOT NULL REFERENCES multipart_uploads(upload_id) ON DELETE CASCADE,
    part_number INT NOT NULL,
    path TEXT NOT NULL,
    content BYTEA NOT NULL,
    PRIMARY KEY (upload_id, part_number)
);
CREATE TABLE IF NOT EXISTS relay_events (
    id BIGSERIAL PRIMARY KEY,
    room TEXT NOT NULL,
    sender TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
"#;

const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS telemetry_events (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID REFERENCES users(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    payload BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS telemetry_events_user ON telemetry_events (user_id, created_at);
CREATE INDEX IF NOT EXISTS telemetry_events_created ON telemetry_events (created_at);
CREATE TABLE IF NOT EXISTS usage_daily (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    day DATE NOT NULL,
    kind TEXT NOT NULL,
    count INT NOT NULL,
    PRIMARY KEY (user_id, day, kind)
);
CREATE TABLE IF NOT EXISTS upstream_buckets (
    name TEXT PRIMARY KEY,
    remaining INT NOT NULL,
    reset_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
"#;

/// API keys are stored as a SHA-256 of the full key; the plaintext is shown once, at creation.
const SCHEMA_V3: &str = r#"
CREATE TABLE IF NOT EXISTS api_keys (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    key_hash TEXT NOT NULL UNIQUE,
    key_suffix TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS api_keys_user ON api_keys (user_id, created_at);
"#;

/// Applied in order, once each, under one advisory lock so instances can start together.
const MIGRATIONS: &[(i32, &str)] = &[(1, SCHEMA), (2, SCHEMA_V2), (3, SCHEMA_V3)];
const MIGRATION_LOCK: i64 = 0x636f_7274_6578;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("database request failed")]
    Query,
    #[error("database is not accepting connections")]
    Connect,
    #[error("signing key is unusable")]
    Key,
}

#[derive(Clone)]
pub struct Db {
    pool: Pool,
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub password_hash: String,
    pub first_name: String,
    pub last_name: String,
    pub opt_out: bool,
    pub locked_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct AuthTx {
    pub state: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub nonce: String,
    pub scope: String,
    pub user_id: Option<Uuid>,
    pub code_hash: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct RefreshRow {
    pub family_id: Uuid,
    pub user_id: Uuid,
    pub scope: String,
}

pub enum RefreshConsume {
    Ok(RefreshRow),
    Reuse(Uuid),
    Invalid,
}

#[derive(Debug, Clone)]
pub struct DeviceGrant {
    pub device_code_hash: String,
    pub user_code: String,
    pub client_id: String,
    pub scope: String,
    pub status: String,
    pub user_id: Option<Uuid>,
    pub expires_at: DateTime<Utc>,
    pub interval_secs: i32,
    pub poll_after: Option<DateTime<Utc>>,
    pub consumed_at: Option<DateTime<Utc>>,
}

/// A key as its owner sees it in the console; the secret itself is never stored.
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: Uuid,
    pub name: String,
    pub key_suffix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// The account and key a request authenticated with.
#[derive(Debug, Clone)]
pub struct ApiKeyPrincipal {
    pub user_id: Uuid,
    pub key: ApiKey,
}

pub struct Rate {
    pub allowed: bool,
    pub retry_after: i32,
}

impl Db {
    pub async fn connect(database_url: &str) -> Result<Self, DbError> {
        ensure_database(database_url).await?;
        let pg_config =
            tokio_postgres::Config::from_str(database_url).map_err(|_| DbError::Connect)?;
        let manager = Manager::from_config(
            pg_config,
            NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(16)
            .runtime(Runtime::Tokio1)
            .build()
            .map_err(|_| DbError::Connect)?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<(), DbError> {
        let mut client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let fail = |err: tokio_postgres::Error| {
            tracing::error!(sqlstate = ?err.code(), "schema migration failed");
            DbError::Query
        };
        let tx = client.transaction().await.map_err(fail)?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
            .await
            .map_err(fail)?;
        tx.batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INT PRIMARY KEY,
                applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .await
        .map_err(fail)?;
        let applied: Vec<i32> = tx
            .query("SELECT version FROM schema_migrations", &[])
            .await
            .map_err(fail)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        for (version, sql) in MIGRATIONS {
            if applied.contains(version) {
                continue;
            }
            tx.batch_execute(sql).await.map_err(fail)?;
            tx.execute(
                "INSERT INTO schema_migrations (version) VALUES ($1)",
                &[version],
            )
            .await
            .map_err(fail)?;
            tracing::info!(version, "migration applied");
        }
        tx.commit().await.map_err(fail)?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .query_one("SELECT 1", &[])
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    pub async fn conn(&self) -> Result<deadpool_postgres::Object, DbError> {
        self.pool.get().await.map_err(|_| DbError::Connect)
    }

    pub async fn load_keys(&self) -> Result<KeySet, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let rows = client
            .query(
                "SELECT kid, pkcs8_der, created_at, retired_at FROM signing_keys ORDER BY created_at",
                &[],
            )
            .await
            .map_err(|_| DbError::Query)?;
        let mut keys = Vec::new();
        for row in rows {
            let der: Vec<u8> = row.get(1);
            let private = jwt::private_from_der(&der).map_err(|_| DbError::Key)?;
            keys.push(SigningKey {
                kid: row.get(0),
                private,
                created_at: row.get(2),
                retired_at: row.get(3),
            });
        }
        if !keys.iter().any(|k| k.retired_at.is_none()) {
            let fresh = jwt::generate_key().map_err(|_| DbError::Key)?;
            insert_key(&client, &fresh).await?;
            keys.push(fresh);
        }
        let active_idx = keys
            .iter()
            .rposition(|k| k.retired_at.is_none())
            .ok_or(DbError::Key)?;
        let rotate = Utc::now() - chrono::Duration::days(jwt::KEY_ROTATE_AFTER_DAYS);
        if keys[active_idx].created_at < rotate {
            let retired = client
                .execute(
                    "UPDATE signing_keys SET retired_at = now() WHERE kid = $1 AND retired_at IS NULL",
                    &[&keys[active_idx].kid],
                )
                .await
                .map_err(|_| DbError::Query)?;
            if retired == 0 {
                // Another instance rotated first; its key is the one to load.
                drop(client);
                return Box::pin(self.load_keys()).await;
            }
            keys[active_idx].retired_at = Some(Utc::now());
            let fresh = jwt::generate_key().map_err(|_| DbError::Key)?;
            insert_key(&client, &fresh).await?;
            keys.push(fresh);
        }
        let grace = Utc::now() - chrono::Duration::hours(1);
        let published = keys
            .iter()
            .filter(|k| k.retired_at.is_none_or(|at| at > grace))
            .map(|k| jwt::public_jwk(&k.kid, &k.private))
            .collect::<Vec<PublicJwk>>();
        let active = keys
            .into_iter()
            .rev()
            .find(|k| k.retired_at.is_none())
            .ok_or(DbError::Key)?;
        Ok(KeySet { active, published })
    }

    pub async fn private_for_kid(&self, kid: &str) -> Result<Option<rsa::RsaPrivateKey>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt("SELECT pkcs8_der FROM signing_keys WHERE kid = $1", &[&kid])
            .await
            .map_err(|_| DbError::Query)?;
        match row {
            Some(row) => {
                let der: Vec<u8> = row.get(0);
                jwt::private_from_der(&der)
                    .map(Some)
                    .map_err(|_| DbError::Key)
            }
            None => Ok(None),
        }
    }

    /// Count one use of a daily per-user quota (`kind` names the meter).
    pub async fn usage_hit(&self, user: Uuid, kind: &str, limit: i32) -> Result<Rate, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_one(
                "INSERT INTO usage_daily AS u (user_id, day, kind, count)
                 VALUES ($1, (now() AT TIME ZONE 'utc')::date, $2, 1)
                 ON CONFLICT (user_id, day, kind) DO UPDATE SET count = u.count + 1
                 RETURNING count,
                   CAST(EXTRACT(EPOCH FROM (date_trunc('day', now() AT TIME ZONE 'utc') + interval '1 day'
                                            - (now() AT TIME ZONE 'utc'))) AS INT)",
                &[&user, &kind],
            )
            .await
            .map_err(|err| {
                tracing::error!(sqlstate = ?err.code(), "usage query failed");
                DbError::Query
            })?;
        let count: i32 = row.get(0);
        let retry_after: i32 = row.get(1);
        Ok(Rate {
            allowed: count <= limit,
            retry_after: retry_after.max(1),
        })
    }

    /// Uses counted today (UTC) for a daily meter, without counting one more.
    pub async fn usage_today(&self, user: Uuid, kind: &str) -> Result<i32, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "SELECT count FROM usage_daily
                 WHERE user_id = $1 AND kind = $2 AND day = (now() AT TIME ZONE 'utc')::date",
                &[&user, &kind],
            )
            .await
            .map_err(|err| {
                tracing::error!(sqlstate = ?err.code(), "usage read failed");
                DbError::Query
            })?;
        Ok(row.map_or(0, |row| row.get(0)))
    }

    /// Keep a telemetry payload for `user` (`None` for the unauthenticated product sink).
    pub async fn telemetry_store(
        &self,
        user: Option<Uuid>,
        kind: &str,
        payload: &[u8],
    ) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "INSERT INTO telemetry_events (user_id, kind, payload) VALUES ($1, $2, $3)",
                &[&user, &kind, &payload],
            )
            .await
            .map_err(|err| {
                tracing::error!(sqlstate = ?err.code(), "telemetry insert failed");
                DbError::Query
            })?;
        Ok(())
    }

    /// Seconds to wait when the shared upstream bucket is empty, else `None`.
    pub async fn bucket_wait(&self, name: &str) -> Result<Option<i32>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "SELECT CAST(CEIL(EXTRACT(EPOCH FROM (reset_at - now()))) AS INT)
                 FROM upstream_buckets WHERE name = $1 AND remaining <= 0 AND reset_at > now()",
                &[&name],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.map(|row| row.get::<_, i32>(0).max(1)))
    }

    /// Record what an upstream last said it has left, so every instance sees it.
    pub async fn bucket_set(
        &self,
        name: &str,
        remaining: i32,
        reset_secs: i32,
    ) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "INSERT INTO upstream_buckets AS b (name, remaining, reset_at, updated_at)
                 VALUES ($1, $2, now() + make_interval(secs => $3::int), now())
                 ON CONFLICT (name) DO UPDATE SET remaining = $2,
                   reset_at = now() + make_interval(secs => $3::int), updated_at = now()",
                &[&name, &remaining, &reset_secs],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    /// Retention and expired-row cleanup. Returns how many rows went.
    pub async fn purge(&self, telemetry_days: i32) -> Result<u64, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let statements = [
            "DELETE FROM telemetry_events WHERE created_at < now() - make_interval(days => $1::int)",
            "DELETE FROM usage_daily WHERE day < (now() AT TIME ZONE 'utc')::date - $1::int",
            "DELETE FROM auth_transactions WHERE expires_at < now() - interval '1 hour' AND $1::int >= 0",
            "DELETE FROM device_grants WHERE expires_at < now() - interval '1 hour' AND $1::int >= 0",
            "DELETE FROM browser_sessions WHERE expires_at < now() AND $1::int >= 0",
            "DELETE FROM refresh_tokens WHERE expires_at < now() - interval '1 day' AND $1::int >= 0",
            "DELETE FROM login_attempts WHERE created_at < now() - interval '7 days' AND $1::int >= 0",
            "DELETE FROM rate_buckets WHERE window_start < now() - interval '1 day' AND $1::int >= 0",
            "DELETE FROM storage_grants WHERE expires_at < now() AND $1::int >= 0",
            "DELETE FROM multipart_uploads WHERE created_at < now() - interval '1 day' AND $1::int >= 0",
            "DELETE FROM relay_events WHERE created_at < now() - interval '1 day' AND $1::int >= 0",
        ];
        let mut total = 0;
        for sql in statements {
            total += client
                .execute(sql, &[&telemetry_days])
                .await
                .map_err(|err| {
                    tracing::error!(sqlstate = ?err.code(), "purge failed");
                    DbError::Query
                })?;
        }
        Ok(total)
    }

    pub async fn hit_rate(&self, key: &str, limit: i32, window_secs: i32) -> Result<Rate, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_one(
                "INSERT INTO rate_buckets AS b (key, hits, window_start)
                 VALUES ($1, 1, now())
                 ON CONFLICT (key) DO UPDATE SET
                   hits = CASE
                     WHEN b.window_start <= now() - make_interval(secs => $2::int) THEN 1
                     ELSE b.hits + 1
                   END,
                   window_start = CASE
                     WHEN b.window_start <= now() - make_interval(secs => $2::int) THEN now()
                     ELSE b.window_start
                   END
                 RETURNING hits,
                   CAST(GREATEST(0, EXTRACT(EPOCH FROM (window_start + make_interval(secs => $2::int) - now()))) AS INT)",
                &[&key, &window_secs],
            )
            .await
            .map_err(|err| {
                tracing::error!(sqlstate = ?err.code(), "rate query failed");
                DbError::Query
            })?;
        let hits: i32 = row.get(0);
        let retry_after: i32 = row.get(1);
        Ok(Rate {
            allowed: hits <= limit,
            retry_after: retry_after.max(1),
        })
    }

    pub async fn lock_retry(
        &self,
        ip: &str,
        email: &str,
        limit: i32,
    ) -> Result<Option<i32>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let user = client
            .query_opt("SELECT locked_until FROM users WHERE email = $1", &[&email])
            .await
            .map_err(|_| DbError::Query)?;
        if let Some(row) = user {
            let locked: Option<DateTime<Utc>> = row.get(0);
            if let Some(until) = locked {
                let secs = (until - Utc::now()).num_seconds();
                if secs > 0 {
                    return Ok(Some(secs.max(1) as i32));
                }
            }
        }
        let count: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM login_attempts
                 WHERE ip = $1 AND succeeded = FALSE AND created_at > now() - interval '15 minutes'",
                &[&ip],
            )
            .await
            .map_err(|_| DbError::Query)?
            .get(0);
        if count >= i64::from(limit) {
            return Ok(Some(60));
        }
        Ok(None)
    }

    pub async fn record_login(
        &self,
        ip: &str,
        email: &str,
        succeeded: bool,
        lock_after: i32,
        lock_secs: i32,
    ) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "INSERT INTO login_attempts (ip, email, succeeded) VALUES ($1, $2, $3)",
                &[&ip, &email, &succeeded],
            )
            .await
            .map_err(|_| DbError::Query)?;
        if succeeded {
            client
                .execute(
                    "UPDATE users SET failed_logins = 0, locked_until = NULL WHERE email = $1",
                    &[&email],
                )
                .await
                .map_err(|_| DbError::Query)?;
        } else {
            client
                .execute(
                    "UPDATE users SET
                       failed_logins = failed_logins + 1,
                       locked_until = CASE
                         WHEN failed_logins + 1 >= $2 THEN now() + make_interval(secs => $3::int)
                         ELSE locked_until
                       END
                     WHERE email = $1",
                    &[&email, &lock_after, &lock_secs],
                )
                .await
                .map_err(|_| DbError::Query)?;
        }
        Ok(())
    }

    pub async fn insert_user(
        &self,
        email: &str,
        password_hash: &str,
        first_name: &str,
        last_name: &str,
    ) -> Result<Uuid, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO users (id, email, password_hash, first_name, last_name) VALUES ($1, $2, $3, $4, $5)",
                &[&id, &email, &password_hash, &first_name, &last_name],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(id)
    }

    pub async fn user_by_email(&self, email: &str) -> Result<Option<User>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "SELECT id, email, password_hash, first_name, last_name, coding_data_retention_opt_out, locked_until
                 FROM users WHERE email = $1",
                &[&email],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.map(user_from_row))
    }

    pub async fn user_by_id(&self, id: Uuid) -> Result<Option<User>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "SELECT id, email, password_hash, first_name, last_name, coding_data_retention_opt_out, locked_until
                 FROM users WHERE id = $1",
                &[&id],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.map(user_from_row))
    }

    pub async fn set_opt_out(&self, id: Uuid, opt_out: bool) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "UPDATE users SET coding_data_retention_opt_out = $2 WHERE id = $1",
                &[&id, &opt_out],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    pub async fn insert_session(
        &self,
        id: &str,
        user_id: Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "INSERT INTO browser_sessions (id, user_id, expires_at) VALUES ($1, $2, $3)",
                &[&id, &user_id, &expires_at],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    /// Stores a new key unless the user already holds `limit` live keys; `false` means the limit was hit.
    pub async fn insert_api_key(
        &self,
        id: Uuid,
        user_id: Uuid,
        name: &str,
        key_hash: &str,
        key_suffix: &str,
        limit: i64,
    ) -> Result<bool, DbError> {
        let mut client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let tx = client.transaction().await.map_err(|_| DbError::Query)?;
        // Serialises concurrent creations for one user so the limit holds.
        tx.execute("SELECT 1 FROM users WHERE id = $1 FOR UPDATE", &[&user_id])
            .await
            .map_err(|_| DbError::Query)?;
        let live: i64 = tx
            .query_one(
                "SELECT count(*) FROM api_keys WHERE user_id = $1 AND revoked_at IS NULL",
                &[&user_id],
            )
            .await
            .map_err(|_| DbError::Query)?
            .get(0);
        if live >= limit {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO api_keys (id, user_id, name, key_hash, key_suffix) VALUES ($1, $2, $3, $4, $5)",
            &[&id, &user_id, &name, &key_hash, &key_suffix],
        )
        .await
        .map_err(|_| DbError::Query)?;
        tx.commit().await.map_err(|_| DbError::Query)?;
        Ok(true)
    }

    /// The user's live keys, newest first.
    pub async fn api_keys_for(&self, user_id: Uuid) -> Result<Vec<ApiKey>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let rows = client
            .query(
                "SELECT id, name, key_suffix, created_at, last_used_at FROM api_keys
                 WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
                &[&user_id],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(rows
            .iter()
            .map(|row| ApiKey {
                id: row.get(0),
                name: row.get(1),
                key_suffix: row.get(2),
                created_at: row.get(3),
                last_used_at: row.get(4),
            })
            .collect())
    }

    /// Revokes one of the user's own keys; `false` when no such live key belongs to them.
    pub async fn revoke_api_key(&self, user_id: Uuid, id: Uuid) -> Result<bool, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let changed = client
            .execute(
                "UPDATE api_keys SET revoked_at = now()
                 WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
                &[&id, &user_id],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(changed == 1)
    }

    /// Resolves a live key by its hash. The last-used time is written at most once a minute,
    /// so a busy key does not turn every request into a row update.
    pub async fn api_key_principal(
        &self,
        key_hash: &str,
    ) -> Result<Option<ApiKeyPrincipal>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let Some(row) = client
            .query_opt(
                "SELECT user_id, id, name, key_suffix, created_at, last_used_at,
                        last_used_at IS NULL OR last_used_at < now() - interval '1 minute'
                 FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
                &[&key_hash],
            )
            .await
            .map_err(|_| DbError::Query)?
        else {
            return Ok(None);
        };
        let mut key = ApiKey {
            id: row.get(1),
            name: row.get(2),
            key_suffix: row.get(3),
            created_at: row.get(4),
            last_used_at: row.get(5),
        };
        let stale: bool = row.get(6);
        if stale {
            let touched = client
                .query_opt(
                    "UPDATE api_keys SET last_used_at = now()
                     WHERE id = $1 AND revoked_at IS NULL RETURNING last_used_at",
                    &[&key.id],
                )
                .await
                .map_err(|_| DbError::Query)?;
            match touched {
                Some(touched) => key.last_used_at = touched.get(0),
                // Revoked between the two statements.
                None => return Ok(None),
            }
        }
        Ok(Some(ApiKeyPrincipal {
            user_id: row.get(0),
            key,
        }))
    }

    pub async fn session_user(&self, id: &str) -> Result<Option<Uuid>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "SELECT user_id FROM browser_sessions WHERE id = $1 AND expires_at > now()",
                &[&id],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.map(|row| row.get(0)))
    }

    pub async fn save_auth_tx(&self, tx: &AuthTx) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "INSERT INTO auth_transactions
                   (state, client_id, redirect_uri, code_challenge, nonce, scope, user_id, code_hash, expires_at, used_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (state) DO UPDATE SET
                   client_id = EXCLUDED.client_id,
                   redirect_uri = EXCLUDED.redirect_uri,
                   code_challenge = EXCLUDED.code_challenge,
                   nonce = EXCLUDED.nonce,
                   scope = EXCLUDED.scope,
                   expires_at = EXCLUDED.expires_at
                 WHERE auth_transactions.used_at IS NULL AND auth_transactions.code_hash IS NULL",
                &[
                    &tx.state,
                    &tx.client_id,
                    &tx.redirect_uri,
                    &tx.code_challenge,
                    &tx.nonce,
                    &tx.scope,
                    &tx.user_id,
                    &tx.code_hash,
                    &tx.expires_at,
                    &tx.used_at,
                ],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    pub async fn auth_tx(&self, state: &str) -> Result<Option<AuthTx>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "SELECT state, client_id, redirect_uri, code_challenge, nonce, scope, user_id, code_hash, expires_at, used_at
                 FROM auth_transactions WHERE state = $1",
                &[&state],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.as_ref().map(auth_tx_from_row))
    }

    pub async fn issue_code(
        &self,
        state: &str,
        user_id: Uuid,
        code_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<bool, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let n = client
            .execute(
                "UPDATE auth_transactions
                 SET user_id = $2, code_hash = $3, expires_at = $4
                 WHERE state = $1 AND used_at IS NULL AND code_hash IS NULL",
                &[&state, &user_id, &code_hash, &expires_at],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(n == 1)
    }

    pub async fn consume_code(&self, code_hash: &str) -> Result<Option<AuthTx>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "UPDATE auth_transactions SET used_at = now()
                 WHERE code_hash = $1 AND used_at IS NULL AND expires_at > now() AND user_id IS NOT NULL
                 RETURNING state, client_id, redirect_uri, code_challenge, nonce, scope, user_id, code_hash, expires_at, used_at",
                &[&code_hash],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.as_ref().map(auth_tx_from_row))
    }

    pub async fn insert_refresh(
        &self,
        token_hash: &str,
        family_id: Uuid,
        user_id: Uuid,
        scope: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "INSERT INTO refresh_tokens (token_hash, family_id, user_id, scope, expires_at)
                 VALUES ($1, $2, $3, $4, $5)",
                &[&token_hash, &family_id, &user_id, &scope, &expires_at],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    pub async fn consume_refresh(&self, token_hash: &str) -> Result<RefreshConsume, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "UPDATE refresh_tokens SET used_at = now()
                 WHERE token_hash = $1 AND used_at IS NULL AND revoked_at IS NULL AND expires_at > now()
                 RETURNING family_id, user_id, scope",
                &[&token_hash],
            )
            .await
            .map_err(|_| DbError::Query)?;
        if let Some(row) = row {
            return Ok(RefreshConsume::Ok(RefreshRow {
                family_id: row.get(0),
                user_id: row.get(1),
                scope: row.get(2),
            }));
        }
        let existing = client
            .query_opt(
                "SELECT family_id, used_at IS NOT NULL FROM refresh_tokens WHERE token_hash = $1",
                &[&token_hash],
            )
            .await
            .map_err(|_| DbError::Query)?;
        match existing {
            Some(row) if row.get::<_, bool>(1) => Ok(RefreshConsume::Reuse(row.get(0))),
            _ => Ok(RefreshConsume::Invalid),
        }
    }

    pub async fn revoke_family(&self, family_id: Uuid) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "UPDATE refresh_tokens SET revoked_at = now() WHERE family_id = $1 AND revoked_at IS NULL",
                &[&family_id],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    pub async fn insert_device(&self, grant: &DeviceGrant) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "INSERT INTO device_grants
                   (device_code_hash, user_code, client_id, scope, status, user_id, expires_at, interval_secs, poll_after)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &grant.device_code_hash,
                    &grant.user_code,
                    &grant.client_id,
                    &grant.scope,
                    &grant.status,
                    &grant.user_id,
                    &grant.expires_at,
                    &grant.interval_secs,
                    &grant.poll_after,
                ],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    pub async fn device_by_hash(&self, hash: &str) -> Result<Option<DeviceGrant>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(&device_select("device_code_hash = $1"), &[&hash])
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.as_ref().map(device_from_row))
    }

    pub async fn device_by_user_code(
        &self,
        user_code: &str,
    ) -> Result<Option<DeviceGrant>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(&device_select("user_code = $1"), &[&user_code])
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.as_ref().map(device_from_row))
    }

    pub async fn mark_device_poll(
        &self,
        hash: &str,
        poll_after: DateTime<Utc>,
    ) -> Result<(), DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        client
            .execute(
                "UPDATE device_grants SET poll_after = $2 WHERE device_code_hash = $1",
                &[&hash, &poll_after],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(())
    }

    pub async fn decide_device(
        &self,
        user_code: &str,
        user_id: Uuid,
        status: &str,
    ) -> Result<bool, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let n = client
            .execute(
                "UPDATE device_grants SET status = $3, user_id = $2
                 WHERE user_code = $1 AND status = 'pending' AND expires_at > now() AND consumed_at IS NULL",
                &[&user_code, &user_id, &status],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(n == 1)
    }

    pub async fn consume_device(&self, hash: &str) -> Result<Option<DeviceGrant>, DbError> {
        let client = self.pool.get().await.map_err(|_| DbError::Connect)?;
        let row = client
            .query_opt(
                "UPDATE device_grants SET consumed_at = now()
                 WHERE device_code_hash = $1 AND status = 'approved' AND consumed_at IS NULL AND expires_at > now()
                 RETURNING device_code_hash, user_code, client_id, scope, status, user_id, expires_at, interval_secs, poll_after, consumed_at",
                &[&hash],
            )
            .await
            .map_err(|_| DbError::Query)?;
        Ok(row.as_ref().map(device_from_row))
    }
}

fn device_select(pred: &str) -> String {
    format!(
        "SELECT device_code_hash, user_code, client_id, scope, status, user_id, expires_at, interval_secs, poll_after, consumed_at
         FROM device_grants WHERE {pred}"
    )
}

fn user_from_row(row: tokio_postgres::Row) -> User {
    User {
        id: row.get(0),
        email: row.get(1),
        password_hash: row.get(2),
        first_name: row.get(3),
        last_name: row.get(4),
        opt_out: row.get(5),
        locked_until: row.get(6),
    }
}

fn auth_tx_from_row(row: &tokio_postgres::Row) -> AuthTx {
    AuthTx {
        state: row.get(0),
        client_id: row.get(1),
        redirect_uri: row.get(2),
        code_challenge: row.get(3),
        nonce: row.get(4),
        scope: row.get(5),
        user_id: row.get(6),
        code_hash: row.get(7),
        expires_at: row.get(8),
        used_at: row.get(9),
    }
}

fn device_from_row(row: &tokio_postgres::Row) -> DeviceGrant {
    DeviceGrant {
        device_code_hash: row.get(0),
        user_code: row.get(1),
        client_id: row.get(2),
        scope: row.get(3),
        status: row.get(4),
        user_id: row.get(5),
        expires_at: row.get(6),
        interval_secs: row.get(7),
        poll_after: row.get(8),
        consumed_at: row.get(9),
    }
}

async fn insert_key(client: &tokio_postgres::Client, key: &SigningKey) -> Result<(), DbError> {
    let der = jwt::private_der(&key.private).map_err(|_| DbError::Key)?;
    client
        .execute(
            "INSERT INTO signing_keys (kid, pkcs8_der, created_at, retired_at) VALUES ($1, $2, $3, $4)",
            &[&key.kid, &der, &key.created_at, &key.retired_at],
        )
        .await
        .map_err(|_| DbError::Query)?;
    Ok(())
}

pub async fn ensure_database(database_url: &str) -> Result<(), DbError> {
    match tokio_postgres::connect(database_url, NoTls).await {
        Ok((client, conn)) => {
            tokio::spawn(async move {
                let _ = conn.await;
            });
            drop(client);
            Ok(())
        }
        Err(err) => {
            let missing = err
                .code()
                .is_some_and(|code| code == &tokio_postgres::error::SqlState::INVALID_CATALOG_NAME);
            if !missing {
                tracing::error!(sqlstate = ?err.code(), "database connection failed");
                return Err(DbError::Connect);
            }
            let (admin_url, name) = admin_target(database_url).map_err(|_| DbError::Connect)?;
            let (client, conn) = tokio_postgres::connect(&admin_url, NoTls)
                .await
                .map_err(|_| DbError::Connect)?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let statement = format!("CREATE DATABASE {name}");
            if let Err(create_err) = client.batch_execute(&statement).await {
                let duplicate = create_err.code().is_some_and(|code| code.code() == "42P04");
                if !duplicate {
                    tracing::error!(sqlstate = ?create_err.code(), "create database failed");
                    return Err(DbError::Query);
                }
            }
            Ok(())
        }
    }
}

fn admin_target(database_url: &str) -> Result<(String, String), ()> {
    let mut parsed = url::Url::parse(database_url).map_err(|_| ())?;
    let name = parsed.path().trim_start_matches('/').to_owned();
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.is_empty() {
        return Err(());
    }
    parsed.set_path("/postgres");
    Ok((parsed.to_string(), name))
}
