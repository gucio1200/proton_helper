use futures::{Stream, StreamExt};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    api::{Api, Patch, PatchParams},
    runtime::{
        controller::{Action, Controller},
        reflector,
        reflector::ObjectRef,
        watcher,
        watcher::Config,
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
use warp::Filter;

// --- 1. CONSTANTS & METRICS ---
const TAINT_KEY: &str = "multus.network.k8s.io/readiness";
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
    let namespace = env::var("NAMESPACE").unwrap_or_else(|_| "kube-system".to_string());
    let selector = env::var("MULTUS_LABEL_SELECTOR").unwrap_or_else(|_| "app=multus".to_string());
    let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string());
    let client = Client::try_default().await?;

    // B. Metrics Server Setup
    // FIX: Define routes and create the server future *before* spawning.
    // This prevents the compiler from confusing lifetimes inside an async block.
    let metrics_route = warp::path("metrics").and(warp::get()).map(|| {
        let encoder = TextEncoder::new();
        let families = prometheus::gather();
        let mut buffer = vec![];
        encoder.encode(&families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    });
    
    let health_route = warp::path("health").and(warp::get()).map(|| "ok".to_string());
    
    let routes = health_route.or(metrics_route);
    let server_future = warp::serve(routes).run(([0, 0, 0, 0], 8080));
    
    // FIX: Spawn the future directly. Do not wrap it in 'async move { ... }'
    tokio::spawn(server_future);
    
    // C. Leader Election
    let is_leader = start_leader_election(client.clone(), &namespace, &hostname);

    // D. Cache Setup
    let (pod_store, pod_reflector_stream) = setup_pod_cache(client.clone(), &selector).await;
    tokio::spawn(async move {
        pod_reflector_stream.for_each(|_| async {}).await;
    });

    // E. Shared Context
    let context = Arc::new(Context {
        client: client.clone(),
        pod_store,
        is_leader,
    });

    // F. Run Controller
    tracing::info!("🚀 Starting Multus Controller");
    let nodes_api = Api::<Node>::all(client.clone());
    let pods_api = Api::<Pod>::all(client.clone());
    let pod_config = Config::default().labels(&selector).fields("status.phase=Running");

    Controller::new(nodes_api, Config::default())
        .with_config(kube::runtime::controller::Config::default().concurrency(5))
        .watches(
            pods_api,
            pod_config,
            |pod: Pod| {
                pod.spec.as_ref()
                    .and_then(|s| s.node_name.clone())
                    .map(|name| ObjectRef::<Node>::new(name.as_str()))
            },
        )
        .run(reconcile, error_policy, context)
        .for_each(|_| async {})
        .await;

    Ok(())
}

// --- 3. RECONCILIATION LOGIC ---
struct Context {
    client: Client,
    pod_store: reflector::Store<Pod>,
    is_leader: Arc<AtomicBool>,
}

async fn reconcile(node: Arc<Node>, ctx: Arc<Context>) -> Result<Action, kube::Error> {
    if !ctx.is_leader.load(Ordering::Relaxed) {
        return Ok(Action::await_change());
    }

    let _timer = RECONCILE_DURATION.start_timer();
    let node_name = node.name_any();
    
    // Check Memory Cache
    let is_multus_ready = ctx.pod_store.state().iter().any(|pod| {
        pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(&node_name)
    });

    // Apply Logic
    ensure_taint(&ctx.client, &node, !is_multus_ready).await?;

    Ok(Action::await_change())
}

async fn ensure_taint(client: &Client, node: &Node, want_taint: bool) -> Result<(), kube::Error> {
    let api: Api<Node> = Api::all(client.clone());
    let node_name = node.name_any();

    for i in 0..3 {
        let target_node = if i == 0 { node } else { &api.get(&node_name).await? };
        
        let current_taints = target_node.spec.as_ref().and_then(|s| s.taints.clone()).unwrap_or_default();
        let has_taint = current_taints.iter().any(|t| t.key == TAINT_KEY);

        if has_taint == want_taint { return Ok(()); }

        let mut new_taints = current_taints.clone();
        if want_taint {
            new_taints.push(k8s_openapi::api::core::v1::Taint {
                key: TAINT_KEY.to_string(),
                value: Some("false".to_string()),
                effect: "NoSchedule".to_string(),
                time_added: None,
            });
        } else {
            new_taints.retain(|t| t.key != TAINT_KEY);
        }

        let patch_json = json!([
            { "op": "test", "path": "/metadata/resourceVersion", "value": target_node.resource_version() },
            { "op": "replace", "path": "/spec/taints", "value": new_taints }
        ]);
        
        let patch: json_patch::Patch = serde_json::from_value(patch_json).map_err(kube::Error::SerdeError)?;

        match api.patch(&node_name, &PatchParams::default(), &Patch::<()>::Json(patch)).await {
            Ok(_) => {
                TAINT_OPERATIONS.inc();
                tracing::info!(node = %node_name, action = %if want_taint { "TAINTED" } else { "UNTAINTED" }, "State updated");
                return Ok(());
            },
            Err(kube::Error::Api(ae)) if ae.code == 409 || ae.code == 422 => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn error_policy(_node: Arc<Node>, _err: &kube::Error, _ctx: Arc<Context>) -> Action {
    Action::requeue(Duration::from_secs(5))
}

// --- 4. INFRASTRUCTURE HELPERS ---

async fn setup_pod_cache(client: Client, selector: &str) -> (
    reflector::Store<Pod>, 
    impl Stream<Item = Result<watcher::Event<Pod>, watcher::Error>> + Send + Unpin + 'static
) {
    let api = Api::<Pod>::all(client);
    let (store, writer) = reflector::store();
    let config = Config::default().labels(selector).fields("status.phase=Running");
    
    let mut reflector_stream = reflector::reflector(writer, watcher(api, config)).boxed();
    
    tracing::info!("⏳ Waiting for cache sync...");
    let _ = reflector_stream.next().await; 
    tracing::info!("✅ Cache synced");

    (store, reflector_stream)
}

fn start_leader_election(client: Client, ns: &str, hostname: &str) -> Arc<AtomicBool> {
    let is_leader = Arc::new(AtomicBool::new(false));
    let flag = is_leader.clone();
    let params = LeaseLockParams {
        holder_id: hostname.to_string(),
        lease_name: LEASE_NAME.to_string(),
        lease_ttl: Duration::from_secs(15),
    };
    let lock = LeaseLock::new(client, ns, params);

    tokio::spawn(async move {
        loop {
            match lock.try_acquire_or_renew().await {
                Ok(lease) => {
                    if lease.acquired_lease != flag.load(Ordering::Relaxed) {
                        tracing::info!("👑 Leadership status changed: {}", lease.acquired_lease);
                        flag.store(lease.acquired_lease, Ordering::Relaxed);
                    }
                },
                Err(e) => tracing::error!("Leader Election failure: {}", e),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    is_leader
}
