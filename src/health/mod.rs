use chrono::Utc;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct ComponentHealth {
    pub status: String,
    pub updated_at: String,
    pub last_ok: Option<String>,
    pub last_error: Option<String>,
    pub restart_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub pid: u32,
    pub updated_at: String,
    pub uptime_seconds: u64,
    pub components: BTreeMap<String, ComponentHealth>,
}

impl HealthSnapshot {
    /// Names of components currently in the `error` state (failed and awaiting
    /// restart). Drives the `/readyz` readiness verdict so an orchestrator can
    /// take a crash-looping instance out of rotation. A `starting` component is
    /// not counted — it is initializing, not broken.
    pub fn unhealthy_components(&self) -> Vec<String> {
        self.components
            .iter()
            .filter(|(_, c)| c.status == "error")
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// True when no supervised component is in the `error` state.
    pub fn is_ready(&self) -> bool {
        self.unhealthy_components().is_empty()
    }

    /// A public-safe view for the unauthenticated `/health` and `/readyz`
    /// endpoints: the readiness verdict plus the *names* of unhealthy
    /// components only. No pid, no uptime, and no raw `last_error` strings —
    /// those ride on the bearer-gated `/api/v1/status.runtime`.
    pub fn public_status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ready": self.unhealthy_components().is_empty(),
            "unhealthy_components": self.unhealthy_components(),
        })
    }
}

struct HealthRegistry {
    started_at: Instant,
    components: Mutex<BTreeMap<String, ComponentHealth>>,
}

static REGISTRY: OnceLock<HealthRegistry> = OnceLock::new();

fn registry() -> &'static HealthRegistry {
    REGISTRY.get_or_init(|| HealthRegistry {
        started_at: Instant::now(),
        components: Mutex::new(BTreeMap::new()),
    })
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn upsert_component<F>(component: &str, update: F)
where
    F: FnOnce(&mut ComponentHealth),
{
    let mut map = registry().components.lock();
    let now = now_rfc3339();
    let entry = map
        .entry(component.to_string())
        .or_insert_with(|| ComponentHealth {
            status: "starting".into(),
            updated_at: now.clone(),
            last_ok: None,
            last_error: None,
            restart_count: 0,
        });
    update(entry);
    entry.updated_at = now;
}

pub fn mark_component_ok(component: &str) {
    upsert_component(component, |entry| {
        entry.status = "ok".into();
        entry.last_ok = Some(now_rfc3339());
        entry.last_error = None;
    });
}

#[allow(clippy::needless_pass_by_value)]
pub fn mark_component_error(component: &str, error: impl ToString) {
    // Scrub secret-looking tokens before storing: even the bearer-gated
    // `/api/v1/status.runtime` view should never keep a credentialed URL a
    // component's error `Display` happened to embed.
    let err = crate::providers::sanitize_api_error(&error.to_string());
    upsert_component(component, move |entry| {
        entry.status = "error".into();
        entry.last_error = Some(err);
    });
}

pub fn bump_component_restart(component: &str) {
    upsert_component(component, |entry| {
        entry.restart_count = entry.restart_count.saturating_add(1);
    });
}

pub fn snapshot() -> HealthSnapshot {
    let components = registry().components.lock().clone();

    HealthSnapshot {
        pid: std::process::id(),
        updated_at: now_rfc3339(),
        uptime_seconds: registry().started_at.elapsed().as_secs(),
        components,
    }
}

pub fn snapshot_json() -> serde_json::Value {
    serde_json::to_value(snapshot()).unwrap_or_else(|_| {
        serde_json::json!({
            "status": "error",
            "message": "failed to serialize health snapshot"
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_component(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn public_status_json_omits_pid_and_component_details() {
        // The unauthenticated /health and /readyz views must not carry the pid,
        // uptime, or raw component error strings.
        let snap = snapshot();
        let public = snap.public_status_json();
        assert!(public.get("ready").is_some());
        assert!(public.get("unhealthy_components").is_some());
        assert!(
            public.get("pid").is_none(),
            "public view must not expose pid"
        );
        assert!(
            public.get("components").is_none(),
            "public view must not expose per-component details"
        );
        assert!(public.get("uptime_seconds").is_none());
    }

    #[test]
    fn mark_component_ok_initializes_component_state() {
        let component = unique_component("health-ok");

        mark_component_ok(&component);

        let snapshot = snapshot();
        let entry = snapshot
            .components
            .get(&component)
            .expect("component should be present after mark_component_ok");

        assert_eq!(entry.status, "ok");
        assert!(entry.last_ok.is_some());
        assert!(entry.last_error.is_none());
    }

    #[test]
    fn mark_component_error_then_ok_clears_last_error() {
        let component = unique_component("health-error");

        mark_component_error(&component, "first failure");
        let error_snapshot = snapshot();
        let errored = error_snapshot
            .components
            .get(&component)
            .expect("component should exist after mark_component_error");
        assert_eq!(errored.status, "error");
        assert_eq!(errored.last_error.as_deref(), Some("first failure"));

        mark_component_ok(&component);
        let recovered_snapshot = snapshot();
        let recovered = recovered_snapshot
            .components
            .get(&component)
            .expect("component should exist after recovery");
        assert_eq!(recovered.status, "ok");
        assert!(recovered.last_error.is_none());
        assert!(recovered.last_ok.is_some());
    }

    #[test]
    fn bump_component_restart_increments_counter() {
        let component = unique_component("health-restart");

        bump_component_restart(&component);
        bump_component_restart(&component);

        let snapshot = snapshot();
        let entry = snapshot
            .components
            .get(&component)
            .expect("component should exist after restart bump");

        assert_eq!(entry.restart_count, 2);
    }

    #[test]
    fn snapshot_json_contains_registered_component_fields() {
        let component = unique_component("health-json");

        mark_component_ok(&component);

        let json = snapshot_json();
        let component_json = &json["components"][&component];

        assert_eq!(component_json["status"], "ok");
        assert!(component_json["updated_at"].as_str().is_some());
        assert!(component_json["last_ok"].as_str().is_some());
        assert!(json["uptime_seconds"].as_u64().is_some());
    }

    // Readiness is a pure function of a snapshot — build snapshots directly so
    // these tests don't touch the process-global registry.
    fn component(status: &str) -> ComponentHealth {
        ComponentHealth {
            status: status.into(),
            updated_at: "t".into(),
            last_ok: None,
            last_error: None,
            restart_count: 0,
        }
    }

    fn snapshot_with(entries: &[(&str, &str)]) -> HealthSnapshot {
        HealthSnapshot {
            pid: 1,
            updated_at: "t".into(),
            uptime_seconds: 0,
            components: entries
                .iter()
                .map(|(name, status)| ((*name).to_string(), component(status)))
                .collect(),
        }
    }

    #[test]
    fn readiness_ready_when_all_ok_or_starting() {
        let snap = snapshot_with(&[("gateway", "ok"), ("channels", "starting")]);
        assert!(snap.is_ready());
        assert!(snap.unhealthy_components().is_empty());
    }

    #[test]
    fn readiness_not_ready_lists_errored_components() {
        let snap = snapshot_with(&[("gateway", "ok"), ("channels", "error"), ("cron", "error")]);
        assert!(!snap.is_ready());
        assert_eq!(
            snap.unhealthy_components(),
            vec!["channels".to_string(), "cron".to_string()]
        );
    }

    #[test]
    fn readiness_ready_when_no_components() {
        assert!(snapshot_with(&[]).is_ready());
    }
}
