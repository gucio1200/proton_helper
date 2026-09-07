use futures::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod, Taint};
use kube::{
    api::{Api, ListParams, Patch, PatchParams},
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
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tracing_subscriber::EnvFilter;
use warp::Filter;

mod taints;
use taints::{has_managed_taint, is_multus_ready, node_taints, plan_taints, DEFAULT_TAINT_KEY};

const LEASE_NAME: &str = "multus-controller-leader";
const LEASE_TTL: Duration = Duration::from_secs(15);
const LEASE_INTERVAL: Duration = Duration::from_secs(5);
const REQUEUE_INTERVAL: Duration = Duration::from_secs(60);
const CONFLICT_REQUEUE: Duration = Duration::from_secs(1);
const ERROR_REQUEUE: Duration = Duration::from_secs(5);
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
    serve_http(controller.store(), taint_key.clone());

    let lock = LeaseLock::new(
        client.clone(),
        &namespace,
        LeaseLockParams {
            holder_id: hostname.clone(),
            lease_name: LEASE_NAME.to_string(),
            lease_ttl: LEASE_TTL,
        },
    );
    tracing::info!(holder = %hostname, "waiting for leader lease");
    wait_for_leadership(&lock).await;
    IS_LEADER.set(1);
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
        reason = keep_leadership(&lock) => {
            IS_LEADER.set(0);
            anyhow::bail!("leadership lost: {reason}")
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
    let want_taint = !multus_ready_on(&ctx, &name).await?;

    // The cached node is enough to see that nothing needs to change; a stale hit is re-run by the watch event that follows every patch.
    if plan_taints(node_taints(&node), want_taint, &ctx.taint_key).is_none() {
        return Ok(Action::requeue(REQUEUE_INTERVAL));
    }
    let fresh = ctx.nodes.get(&name).await?;
    if let Some(new_taints) = plan_taints(node_taints(&fresh), want_taint, &ctx.taint_key) {
        match patch_taints(&ctx, &fresh, new_taints).await {
            Ok(()) => {
                let op = if want_taint { "add" } else { "remove" };
                TAINT_OPERATIONS.with_label_values(&[op]).inc();
                tracing::info!(node = %name, op, "taint updated");
                publish_event(&ctx, &fresh, want_taint).await;
            }
            // Someone else changed the node first, so retry shortly from the new version.
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                return Ok(Action::requeue(CONFLICT_REQUEUE));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(Action::requeue(REQUEUE_INTERVAL))
}

/// Lists the Multus pods on a node fresh, so the decision never depends on cache ordering.
async fn multus_ready_on(ctx: &Context, node: &str) -> Result<bool, kube::Error> {
    let params = ListParams::default()
        .labels(&ctx.selector)
        .fields(&format!("spec.nodeName={node}"));
    let pods = ctx.pods.list(&params).await?;
    let ready = pods.iter().any(is_multus_ready);
    tracing::debug!(
        node,
        multus_pods = pods.items.len(),
        ready,
        "checked multus pods"
    );
    Ok(ready)
}

async fn patch_taints(ctx: &Context, node: &Node, taints: Vec<Taint>) -> Result<(), kube::Error> {
    let patch = json!({
        "metadata": { "resourceVersion": node.resource_version() },
        "spec": { "taints": taints },
    });
    let params = PatchParams {
        field_manager: Some(FIELD_MANAGER.to_string()),
        ..PatchParams::default()
    };
    ctx.nodes
        .patch(&node.name_any(), &params, &Patch::Merge(&patch))
        .await
        .map(|_| ())
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

async fn wait_for_leadership(lock: &LeaseLock) {
    loop {
        match try_lease(lock).await {
            Ok(true) => return,
            Ok(false) => tracing::debug!("lease held by another replica"),
            Err(e) => tracing::warn!(error = %e, "lease acquisition failed"),
        }
        tokio::time::sleep(LEASE_INTERVAL).await;
    }
}

/// Renews the lease until it is lost and returns the reason, so main can exit.
async fn keep_leadership(lock: &LeaseLock) -> String {
    let mut last_renewed = Instant::now();
    loop {
        tokio::time::sleep(LEASE_INTERVAL).await;
        match try_lease(lock).await {
            Ok(true) => last_renewed = Instant::now(),
            Ok(false) => return "lease is held by another replica".to_string(),
            Err(e) if last_renewed.elapsed() >= LEASE_TTL => {
                return format!("renewal failing for {:?}: {e}", last_renewed.elapsed());
            }
            Err(e) => tracing::warn!(error = %e, "lease renewal failed"),
        }
    }
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn serve_http(store: Store<Node>, taint_key: String) {
    let health = warp::path("health").map(|| "ok");
    let metrics = warp::path("metrics").map(move || {
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
    tokio::spawn(warp::serve(health.or(metrics).boxed()).run(([0, 0, 0, 0], 8080)));
}
