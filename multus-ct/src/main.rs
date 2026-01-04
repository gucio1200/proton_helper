use futures::{StreamExt, TryStreamExt};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        reflector,
        reflector::{ObjectRef, Store},
        watcher,
        WatchStreamExt,
    },
    Client, ResourceExt,
};
use kube_leader_election::{LeaseLock, LeaseLockParams};
use prometheus::{register_counter, register_histogram, Counter, Histogram, Encoder, TextEncoder};
use serde_json::json;
use std::{
    env,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    time::Duration,
};
use warp::Filter; // Required for .boxed()

// --- 1. CONSTANTS & METRICS ---
const TAINT_KEY: &str = "CriticalAddonsOnly";
const LEASE_NAME: &str = "multus-controller-leader";

lazy_static::lazy_static! {
    static ref RECONCILE_DURATION: Histogram = register_histogram!(
        "multus_reconcile_duration_seconds", "Duration of node reconciliation"
    ).unwrap();
    static ref TAINT_OPERATIONS: Counter = register_counter!(
        "multus_taint_operations_total", "Total number of taint additions/removals"
    ).unwrap();
}

// --- 2. MAIN APPLICATION ---
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    // A. Configuration
    let namespace = env::var("NAMESPACE").unwrap_or_else(|_| "networking".to_string());
    let selector = env::var("MULTUS_LABEL_SELECTOR").unwrap_or_else(|_| "app.kubernetes.io/name=multus".to_string());
    let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string());
    let client = Client::try_default().await?;

    // B. Metrics & Health Server
    // -----------------------------------------------------------------------
    // FIX: logic for Warp + Tokio
    // 1. Define routes.
    // 2. call .boxed() at the end. This is critical for tokio::spawn.
    // -----------------------------------------------------------------------
    let health_route = warp::path("health").map(|| "ok".to_string());
    
    let metrics_route = warp::path("metrics").map(|| {
        let encoder = TextEncoder::new();
        let families = prometheus::gather();
        let mut buffer = vec![];
        encoder.encode(&families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    });

    // .boxed() erases the complex types and standardizes lifetimes
    let routes = health_route.or(metrics_route).boxed();

    tokio::spawn(async move {
        warp::serve(routes).run(([0, 0, 0, 0], 8080)).await;
    });

    // C. Leader Election
    let is_leader = start_leader_election(client.clone(), &namespace, &hostname);

    // D. Cache Setup
    let pods_api = Api::<Pod>::all(client.clone());
    let pod_config = watcher::Config::default().labels(&selector);
    
    let (pod_store, pod_writer) = reflector::store();
    let pod_watcher = watcher(pods_api.clone(), pod_config.clone());
    let pod_reflector = reflector::reflector(pod_writer, pod_watcher);
    
    tokio::spawn(async move {
        let _ = pod_reflector.applied_objects().try_for_each(|_| async { Ok(()) }).await;
    });

    tracing::info!("🚀 Controller started. Watching Nodes & Pods...");

    // E. Main Controller Loop
    let nodes_api = Api::<Node>::all(client.clone());
    let ctx = Arc::new(Context {
        client: client.clone(),
        pod_store,
        is_leader,
    });

    Controller::new(nodes_api, watcher::Config::default())
        .with_config(kube::runtime::controller::Config::default().concurrency(10))
        .watches(
            pods_api,
            pod_config, 
            |pod| {
                pod.spec.as_ref()
                    .and_then(|s| s.node_name.clone())
                    .map(|name| ObjectRef::<Node>::new(name.as_str()))
            },
        )
        .run(reconcile, error_policy, ctx)
        .for_each(|_| async {})
        .await;

    Ok(())
}

// --- 3. RECONCILIATION LOGIC ---
struct Context {
    client: Client,
    pod_store: Store<Pod>,
    is_leader: Arc<AtomicBool>,
}

async fn reconcile(node: Arc<Node>, ctx: Arc<Context>) -> Result<Action, kube::Error> {
    if !ctx.is_leader.load(Ordering::Relaxed) {
        return Ok(Action::await_change());
    }

    let _timer = RECONCILE_DURATION.start_timer();
    let node_name = node.name_any();
    let client = &ctx.client;

    let is_multus_ready = ctx.pod_store.state().iter().any(|pod| {
        let on_node = pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(&node_name);
        let phase_running = pod.status.as_ref().map(|s| s.phase.as_deref() == Some("Running")).unwrap_or(false);
        
        let conditions_ready = pod.status.as_ref().and_then(|s| s.conditions.as_ref()).map(|conds| {
            conds.iter().any(|c| c.type_ == "Ready" && c.status == "True")
        }).unwrap_or(false);

        on_node && phase_running && conditions_ready
    });

    let should_taint = !is_multus_ready;

    ensure_taint_state(client, &node_name, should_taint).await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn ensure_taint_state(client: &Client, node_name: &str, want_taint: bool) -> Result<(), kube::Error> {
    let nodes: Api<Node> = Api::all(client.clone());
    
    for _ in 0..5 {
        let node = nodes.get(node_name).await?;
        
        let current_taints = node.spec.as_ref()
            .and_then(|s| s.taints.clone())
            .unwrap_or_default();

        let has_taint = current_taints.iter().any(|t| t.key == TAINT_KEY);

        if has_taint == want_taint {
            return Ok(());
        }

        let mut new_taints = current_taints.clone();
        if want_taint {
            if !has_taint {
                tracing::info!("🔒 Tainting node {}", node_name);
                new_taints.push(k8s_openapi::api::core::v1::Taint {
                    key: TAINT_KEY.to_string(),
                    value: None, 
                    effect: "NoSchedule".to_string(),
                    time_added: None,
                });
            }
        } else {
            if has_taint {
                tracing::info!("🔓 Untainting node {}", node_name);
                new_taints.retain(|t| t.key != TAINT_KEY);
            }
        }

        let patch_json = json!({
            "apiVersion": "v1",
            "kind": "Node",
            "metadata": {
                "resourceVersion": node.resource_version(),
            },
            "spec": {
                "taints": new_taints
            }
        });

        let params = PatchParams::default();
        match nodes.patch(node_name, &params, &Patch::Merge(patch_json)).await {
            Ok(_) => {
                TAINT_OPERATIONS.inc();
                return Ok(());
            },
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                tracing::warn!("Conflict updating node {}, retrying...", node_name);
                continue;
            },
            Err(e) => return Err(e),
        }
    }
    
    Err(kube::Error::Api(kube::error::ErrorResponse {
        status: "Failure".to_string(),
        message: "Failed to update node taints after retries".to_string(),
        reason: "Conflict".to_string(),
        code: 500,
    }))
}

// --- 4. HELPERS ---

fn error_policy(_node: Arc<Node>, err: &kube::Error, _ctx: Arc<Context>) -> Action {
    tracing::error!("Reconcile error: {:?}", err);
    Action::requeue(Duration::from_secs(5))
}

fn start_leader_election(client: Client, ns: &str, hostname: &str) -> Arc<AtomicBool> {
    let is_leader = Arc::new(AtomicBool::new(false));
    let flag = is_leader.clone();
    let lease_name = LEASE_NAME.to_string();
    let hostname = hostname.to_string();
    let ns = ns.to_string();

    tokio::spawn(async move {
        loop {
            let params = LeaseLockParams {
                holder_id: hostname.clone(),
                lease_name: lease_name.clone(),
                lease_ttl: Duration::from_secs(15),
            };
            let lock = LeaseLock::new(client.clone(), &ns, params);
            
            match lock.try_acquire_or_renew().await {
                Ok(lease) => {
                    if lease.acquired_lease != flag.load(Ordering::Relaxed) {
                        tracing::info!("👑 Leader State Change: {}", lease.acquired_lease);
                        flag.store(lease.acquired_lease, Ordering::Relaxed);
                    }
                },
                Err(e) => tracing::warn!("Leader election error: {}", e),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    is_leader
}
