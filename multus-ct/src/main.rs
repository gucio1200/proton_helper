use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    api::{Api, ListParams, Patch, PatchParams},
    error::ErrorResponse,
    runtime::{
        controller::{Action, Controller},
        events::{Event, EventType, Recorder, Reporter},
        reflector::{ObjectRef, Store},
        watcher,
    },
    Client, Resource, ResourceExt,
};
use kube_leader_election::{LeaseLock, LeaseLockParams};
use prometheus::{
    register_histogram, register_int_counter, register_int_counter_vec, register_int_gauge,
    Encoder, Histogram, IntCounter, IntCounterVec, IntGauge, TextEncoder,
};
use serde_json::json;
use std::{
    env,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use tracing_subscriber::EnvFilter;
use warp::{http::StatusCode, Filter};

mod taints;
use taints::{has_managed_taint, is_multus_ready, plan_taints, DEFAULT_TAINT_KEY};

const LEASE_NAME: &str = "multus-controller-leader";
const LEASE_TTL: Duration = Duration::from_secs(15);
const LEASE_INTERVAL: Duration = Duration::from_secs(5);
const REQUEUE_INTERVAL: Duration = Duration::from_secs(60);
const ERROR_REQUEUE: Duration = Duration::from_secs(5);
const PATCH_ATTEMPTS: usize = 5;
const FIELD_MANAGER: &str = "multus-ct";

static RECONCILE_DURATION: LazyLock<Histogram> = LazyLock::new(|| {
    register_histogram!(
        "multus_reconcile_duration_seconds",
        "Duration of node reconciliation"
    )
    .unwrap()
});
static TAINT_OPERATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "multus_taint_operations_total",
        "Taint patches applied, by operation",
        &["op"]
    )
    .unwrap()
});
static RECONCILE_ERRORS: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "multus_reconcile_errors_total",
        "Node reconciliations that failed"
    )
    .unwrap()
});
static TAINTED_NODES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "multus_tainted_nodes",
        "Nodes currently carrying the Multus readiness taint"
    )
    .unwrap()
});
static IS_LEADER: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "multus_leader",
        "1 when this replica holds the leader lease"
    )
    .unwrap()
});

/// Registers every metric up front so all series exist on the first scrape.
fn init_metrics() {
    LazyLock::force(&RECONCILE_DURATION);
    LazyLock::force(&TAINT_OPERATIONS);
    LazyLock::force(&RECONCILE_ERRORS);
    LazyLock::force(&TAINTED_NODES);
    LazyLock::force(&IS_LEADER);
}

/// Liveness signal fed by the lease loop, which runs for the whole process lifetime.
struct Health {
    last_beat: Mutex<Instant>,
}

impl Health {
    fn new() -> Self {
        Self {
            last_beat: Mutex::new(Instant::now()),
        }
    }

    fn beat(&self) {
        *self.last_beat.lock().unwrap() = Instant::now();
    }

    fn is_alive(&self) -> bool {
        self.last_beat.lock().unwrap().elapsed() < LEASE_INTERVAL * 4
    }
}

struct Context {
    nodes: Api<Node>,
    pods: Api<Pod>,
    selector: String,
    taint_key: String,
    recorder: Recorder,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    init_metrics();

    let namespace = env::var("NAMESPACE").unwrap_or_else(|_| "networking".to_string());
    let selector = env::var("MULTUS_LABEL_SELECTOR")
        .unwrap_or_else(|_| "app.kubernetes.io/name=multus".to_string());
    let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string());
    let taint_key = env::var("TAINT_KEY").unwrap_or_else(|_| DEFAULT_TAINT_KEY.to_string());
    let client = Client::try_default().await?;

    let nodes = Api::<Node>::all(client.clone());
    let pods = Api::<Pod>::all(client.clone());
    let pod_config = watcher::Config::default().labels(&selector);

    // Watches are lazy, so nothing is opened until run() is awaited after leadership.
    let controller = Controller::new(nodes.clone(), watcher::Config::default())
        .with_config(kube::runtime::controller::Config::default().concurrency(10))
        .watches(pods.clone(), pod_config, |pod| {
            pod.spec
                .as_ref()
                .and_then(|s| s.node_name.as_deref())
                .map(ObjectRef::<Node>::new)
        });

    let health = Arc::new(Health::new());
    serve_http(health.clone(), controller.store(), taint_key.clone());

    let lock = Arc::new(LeaseLock::new(
        client.clone(),
        &namespace,
        LeaseLockParams {
            holder_id: hostname.clone(),
            lease_name: LEASE_NAME.to_string(),
            lease_ttl: LEASE_TTL,
        },
    ));
    tracing::info!(holder = %hostname, "waiting for leader lease");
    wait_for_leadership(&lock, &health).await;
    IS_LEADER.set(1);
    let lost = keep_leadership(lock.clone(), health.clone());
    tracing::info!(taint_key = %taint_key, selector = %selector, "acquired lease, starting controller");

    let ctx = Arc::new(Context {
        nodes,
        pods,
        selector,
        taint_key,
        recorder: Recorder::new(
            client,
            Reporter {
                controller: FIELD_MANAGER.to_string(),
                instance: Some(hostname),
            },
        ),
    });
    let run = controller
        .run(reconcile, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                tracing::warn!(error = %e, "controller stream error");
            }
        });

    tokio::select! {
        _ = run => anyhow::bail!("controller stream ended unexpectedly"),
        reason = lost => {
            anyhow::bail!("leadership lost: {}", reason.unwrap_or_else(|_| "lease task died".to_string()))
        }
        _ = shutdown_signal() => {
            tracing::info!("shutting down, releasing lease");
            if let Err(e) = lock.step_down().await {
                tracing::warn!(error = %e, "failed to release lease");
            }
            Ok(())
        }
    }
}

async fn reconcile(node: Arc<Node>, ctx: Arc<Context>) -> Result<Action, kube::Error> {
    let _timer = RECONCILE_DURATION.start_timer();
    let name = node.name_any();

    // Pods are read fresh so the decision never depends on cache ordering.
    let params = ListParams::default()
        .labels(&ctx.selector)
        .fields(&format!("spec.nodeName={name}"));
    let pods = ctx.pods.list(&params).await?;
    let ready = pods.iter().any(is_multus_ready);
    tracing::debug!(node = %name, multus_pods = pods.items.len(), ready, "evaluated node");

    ensure_taint_state(&ctx, &name, !ready).await?;
    Ok(Action::requeue(REQUEUE_INTERVAL))
}

/// Reads the live node and patches its taints under an optimistic lock, retrying on conflicts.
async fn ensure_taint_state(
    ctx: &Context,
    name: &str,
    want_taint: bool,
) -> Result<(), kube::Error> {
    let params = PatchParams {
        field_manager: Some(FIELD_MANAGER.to_string()),
        ..PatchParams::default()
    };
    for attempt in 1..=PATCH_ATTEMPTS {
        let node = ctx.nodes.get(name).await?;
        let current = node
            .spec
            .as_ref()
            .and_then(|s| s.taints.clone())
            .unwrap_or_default();
        let plan = plan_taints(&current, want_taint, &ctx.taint_key);
        if let Some(foreign) = &plan.blocked_by {
            tracing::warn!(node = %name, key = %foreign.key, value = ?foreign.value, "foreign taint occupies our key, not adding ours");
        }
        let Some(new_taints) = plan.new_taints else {
            return Ok(());
        };

        let patch = json!({
            "metadata": { "resourceVersion": node.resource_version() },
            "spec": { "taints": new_taints },
        });
        match ctx.nodes.patch(name, &params, &Patch::Merge(&patch)).await {
            Ok(_) => {
                let op = if want_taint { "add" } else { "remove" };
                TAINT_OPERATIONS.with_label_values(&[op]).inc();
                tracing::info!(node = %name, op, "taint updated");
                publish_event(ctx, &node, want_taint).await;
                return Ok(());
            }
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                tracing::debug!(node = %name, attempt, "node changed concurrently, retrying");
            }
            Err(e) => return Err(e),
        }
    }
    Err(kube::Error::Api(ErrorResponse {
        status: "Failure".to_string(),
        message: format!("node {name} kept changing during {PATCH_ATTEMPTS} patch attempts"),
        reason: "Conflict".to_string(),
        code: 409,
    }))
}

async fn publish_event(ctx: &Context, node: &Node, want_taint: bool) {
    let (type_, reason, action, verb) = if want_taint {
        (EventType::Warning, "MultusNotReady", "Taint", "added")
    } else {
        (EventType::Normal, "MultusReady", "Untaint", "removed")
    };
    let event = Event {
        type_,
        reason: reason.to_string(),
        action: action.to_string(),
        note: Some(format!("{verb} taint {}", ctx.taint_key)),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&event, &node.object_ref(&())).await {
        tracing::warn!(node = %node.name_any(), error = %e, "failed to publish event");
    }
}

fn error_policy(node: Arc<Node>, err: &kube::Error, _ctx: Arc<Context>) -> Action {
    RECONCILE_ERRORS.inc();
    tracing::error!(node = %node.name_any(), error = %err, "reconcile failed");
    Action::requeue(ERROR_REQUEUE)
}

/// One bounded lease call, so a hung request cannot outlive the lease TTL.
async fn try_lease(lock: &LeaseLock) -> anyhow::Result<bool> {
    let result = tokio::time::timeout(LEASE_INTERVAL, lock.try_acquire_or_renew()).await??;
    Ok(result.acquired_lease)
}

async fn wait_for_leadership(lock: &LeaseLock, health: &Health) {
    loop {
        match try_lease(lock).await {
            Ok(true) => return,
            Ok(false) => tracing::debug!("lease held by another replica"),
            Err(e) => tracing::warn!(error = %e, "lease acquisition failed"),
        }
        health.beat();
        tokio::time::sleep(LEASE_INTERVAL).await;
    }
}

/// Renews the lease in the background and reports when leadership is gone, so main can exit.
fn keep_leadership(lock: Arc<LeaseLock>, health: Arc<Health>) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut last_renewed = Instant::now();
        loop {
            tokio::time::sleep(LEASE_INTERVAL).await;
            let lost = match try_lease(&lock).await {
                Ok(true) => {
                    last_renewed = Instant::now();
                    None
                }
                Ok(false) => Some("lease is held by another replica".to_string()),
                Err(e) if last_renewed.elapsed() >= LEASE_TTL => Some(format!(
                    "renewal failing for {:?}: {e}",
                    last_renewed.elapsed()
                )),
                Err(e) => {
                    tracing::warn!(error = %e, "lease renewal failed");
                    None
                }
            };
            health.beat();
            if let Some(reason) = lost {
                IS_LEADER.set(0);
                let _ = tx.send(reason);
                return;
            }
        }
    });
    rx
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn serve_http(health: Arc<Health>, store: Store<Node>, taint_key: String) {
    let health_route = warp::path("health").map(move || {
        if health.is_alive() {
            warp::reply::with_status("ok", StatusCode::OK)
        } else {
            warp::reply::with_status("lease loop stalled", StatusCode::SERVICE_UNAVAILABLE)
        }
    });
    let metrics_route = warp::path("metrics").map(move || {
        let tainted = store
            .state()
            .iter()
            .filter(|n| has_managed_taint(n, &taint_key))
            .count();
        TAINTED_NODES.set(tainted as i64);
        let mut buffer = Vec::new();
        if let Err(e) = TextEncoder::new().encode(&prometheus::gather(), &mut buffer) {
            tracing::warn!(error = %e, "failed to encode metrics");
        }
        String::from_utf8(buffer).unwrap_or_default()
    });
    let routes = health_route.or(metrics_route).boxed();
    tokio::spawn(warp::serve(routes).run(([0, 0, 0, 0], 8080)));
}
