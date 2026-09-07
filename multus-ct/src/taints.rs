use k8s_openapi::api::core::v1::{Node, Pod, Taint};

pub const DEFAULT_TAINT_KEY: &str = "multus.network.k8s.io/readiness";
pub const TAINT_VALUE: &str = "false";
const EFFECT: &str = "NoSchedule";

fn our_taint(key: &str) -> Taint {
    Taint {
        key: key.to_string(),
        value: Some(TAINT_VALUE.to_string()),
        effect: EFFECT.to_string(),
        time_added: None,
    }
}

/// True for taints this controller owns: our key with our value, or with no value at all.
pub fn is_managed(taint: &Taint, key: &str) -> bool {
    taint.key == key && (taint.value.is_none() || taint.value.as_deref() == Some(TAINT_VALUE))
}

pub fn node_taints(node: &Node) -> &[Taint] {
    node.spec
        .as_ref()
        .and_then(|s| s.taints.as_deref())
        .unwrap_or_default()
}

pub fn has_managed_taint(node: &Node, key: &str) -> bool {
    node_taints(node).iter().any(|t| is_managed(t, key))
}

/// Returns the taint list a node should carry, or None when it already matches. Foreign taints are never touched, even one that occupies our key.
pub fn plan_taints(current: &[Taint], want_taint: bool, key: &str) -> Option<Vec<Taint>> {
    let mut desired: Vec<Taint> = current
        .iter()
        .filter(|t| !is_managed(t, key))
        .cloned()
        .collect();
    if want_taint && !desired.iter().any(|t| t.key == key && t.effect == EFFECT) {
        desired.push(our_taint(key));
    }
    let unchanged = desired.len() == current.len() && desired.iter().all(|t| current.contains(t));
    (!unchanged).then_some(desired)
}

/// A pod counts only while Running, Ready and not terminating.
pub fn is_multus_ready(pod: &Pod) -> bool {
    if pod.metadata.deletion_timestamp.is_some() {
        return false;
    }
    let Some(status) = pod.status.as_ref() else {
        return false;
    };
    let running = status.phase.as_deref() == Some("Running");
    let ready = status
        .conditions
        .iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True");
    running && ready
}

#[cfg(test)]
mod tests;
