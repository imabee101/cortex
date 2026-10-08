//! Periodic work: signing-key rotation, retention, and expired-row cleanup.
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::AppState;

pub fn spawn(state: AppState) {
    let keys_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(
            keys_state.config.key_reload_secs.max(1),
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            match keys_state.db.load_keys().await {
                Ok(fresh) => {
                    let mut guard = match keys_state.keys.write() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    *guard = Arc::new(fresh);
                }
                Err(_) => tracing::error!("signing key reload failed"),
            }
        }
    });
    let props_state = state.clone();
    tokio::spawn(async move {
        let Some(origin) = props_state.config.llama_url.clone() else {
            return;
        };
        let mut tick = tokio::time::interval(Duration::from_secs(
            props_state.config.props_refresh_secs.max(1),
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let key = props_state.config.llama_api_key.as_deref();
            let props = crate::inference::fetch_props(&props_state.http, &origin, key).await;
            if props.n_ctx > 0 {
                props_state
                    .slot_context
                    .store(props.n_ctx, Ordering::Relaxed);
                props_state.vision.store(props.vision, Ordering::Relaxed);
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(state.config.purge_secs.max(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match state.db.purge(state.config.telemetry_retention_days).await {
                Ok(rows) => tracing::info!(rows, "retention purge"),
                Err(_) => tracing::error!("retention purge failed"),
            }
        }
    });
}
