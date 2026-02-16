use dashmap::DashMap;
use kube::ResourceExt;
use std::sync::Arc;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::watcher::Event;
use std::collections::HashSet;

#[derive(Clone, Default)]
pub struct NodeIndex {
    // Map: NodeName -> Set of "Ready" Pod UIDs
    ready_pods: Arc<DashMap<String, HashSet<String>>>,
}

impl NodeIndex {
    pub fn new() -> Self {
        Self {
            ready_pods: Arc::new(DashMap::new()),
        }
    }

    /// Check if a node has at least one ready Multus pod (O(1))
    pub fn is_node_ready(&self, node_name: &str) -> bool {
        if let Some(set) = self.ready_pods.get(node_name) {
            !set.is_empty()
        } else {
            false
        }
    }

    /// Process a watcher event to update the index
    pub fn update(&self, event: &Event<Pod>) {
        match event {
            Event::Apply(pod) => self.handle_pod(pod),
            Event::Delete(pod) => self.handle_pod_delete(pod),
            Event::InitApply(pod) => self.handle_pod(pod),
            Event::Init => {
                // Initial sync handled by individual InitApply calls
                // If reflector restarts, we might want to clear, but reflector handles diffs.
            },
            Event::InitDone => {},
        }
    }

    fn handle_pod(&self, pod: &Pod) {
        let node_name = match pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) {
            Some(n) => n.to_string(),
            None => return, // Pod not assigned to a node yet
        };

        let uid = match pod.uid() {
            Some(u) => u,
            None => return,
        };

        let is_ready = self.check_pod_readiness(pod);

        if is_ready {
            // Add to index
            self.ready_pods.entry(node_name).or_default().insert(uid);
        } else {
            // Remove from index (it was ready, now it's not)
            if let Some(mut set) = self.ready_pods.get_mut(&node_name) {
                set.remove(&uid);
                // Clean up empty sets to save memory? Optional, but good practice.
                 if set.is_empty() {
                    // We can't remove the entry while holding a reference to it easily in DashMap 
                    // without a second lookup or using `remove_if`.
                    // DashMap `retain` is heavy. Leaving empty set is fine.
                }
            }
        }
    }

    fn handle_pod_delete(&self, pod: &Pod) {
        let node_name = match pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) {
            Some(n) => n.to_string(),
            None => return,
        };
        let uid = match pod.uid() {
            Some(u) => u,
            None => return,
        };

        if let Some(mut set) = self.ready_pods.get_mut(&node_name) {
            set.remove(&uid);
        }
    }
    
    fn check_pod_readiness(&self, pod: &Pod) -> bool {
        let phase_running = pod.status.as_ref().map(|s| s.phase.as_deref() == Some("Running")).unwrap_or(false);
        let conditions_ready = pod.status.as_ref().and_then(|s| s.conditions.as_ref()).map(|conds| {
            conds.iter().any(|c| c.type_ == "Ready" && c.status == "True")
        }).unwrap_or(false);
        phase_running && conditions_ready
    }
}
