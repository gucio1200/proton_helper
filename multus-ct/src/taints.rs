use k8s_openapi::api::core::v1::{Node, Pod, Taint};

pub const DEFAULT_TAINT_KEY: &str = "multus.network.k8s.io/readiness";
pub const TAINT_VALUE: &str = "false";
const EFFECT: &str = "NoSchedule";

/// Keys written by earlier releases. The flag marks keys shared with other tools, where only the valueless form is ours.
const LEGACY_KEYS: &[(&str, bool)] = &[
    ("multus.network.k8s.io/readiness", false),
    ("CriticalAddonsOnly", true),
];

pub struct Plan {
    /// Taint list to write, or None when the node already matches.
    pub new_taints: Option<Vec<Taint>>,
    /// A foreign taint that occupies our key and effect, so ours cannot be added.
    pub blocked_by: Option<Taint>,
}

fn our_taint(key: &str) -> Taint {
    Taint {
        key: key.to_string(),
        value: Some(TAINT_VALUE.to_string()),
        effect: EFFECT.to_string(),
        time_added: None,
    }
}

/// True for taints this controller owns, including ones left by earlier releases.
pub fn is_managed(taint: &Taint, key: &str) -> bool {
    if taint.key == key {
        return taint.value.is_none() || taint.value.as_deref() == Some(TAINT_VALUE);
    }
    LEGACY_KEYS
        .iter()
        .any(|(k, valueless_only)| taint.key == *k && (!valueless_only || taint.value.is_none()))
}

pub fn has_managed_taint(node: &Node, key: &str) -> bool {
    node.spec
        .iter()
        .flat_map(|s| s.taints.iter().flatten())
        .any(|t| is_managed(t, key))
}

/// Computes the taint list a node should carry, leaving every foreign taint untouched.
pub fn plan_taints(current: &[Taint], want_taint: bool, key: &str) -> Plan {
    let mut desired: Vec<Taint> = current
        .iter()
        .filter(|t| !is_managed(t, key))
        .cloned()
        .collect();
    let mut blocked_by = None;
    if want_taint {
        match desired.iter().find(|t| t.key == key && t.effect == EFFECT) {
            Some(foreign) => blocked_by = Some(foreign.clone()),
            None => desired.push(our_taint(key)),
        }
    }
    let unchanged = desired.len() == current.len() && desired.iter().all(|t| current.contains(t));
    Plan {
        new_taints: (!unchanged).then_some(desired),
        blocked_by,
    }
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
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    fn taint(key: &str, value: Option<&str>, effect: &str) -> Taint {
        Taint {
            key: key.into(),
            value: value.map(Into::into),
            effect: effect.into(),
            time_added: None,
        }
    }

    fn unreachable() -> Taint {
        taint("node.kubernetes.io/unreachable", None, "NoExecute")
    }

    fn aks_system() -> Taint {
        taint("CriticalAddonsOnly", Some("true"), "NoSchedule")
    }

    #[test]
    fn adds_taint_when_missing() {
        let plan = plan_taints(&[unreachable()], true, DEFAULT_TAINT_KEY);
        let new = plan.new_taints.expect("patch expected");
        assert_eq!(new, vec![unreachable(), our_taint(DEFAULT_TAINT_KEY)]);
        assert!(plan.blocked_by.is_none());
    }

    #[test]
    fn noop_when_present_regardless_of_order() {
        let current = [our_taint(DEFAULT_TAINT_KEY), unreachable()];
        assert!(plan_taints(&current, true, DEFAULT_TAINT_KEY)
            .new_taints
            .is_none());
    }

    #[test]
    fn noop_when_absent_and_ready() {
        assert!(plan_taints(&[unreachable()], false, DEFAULT_TAINT_KEY)
            .new_taints
            .is_none());
    }

    #[test]
    fn removes_own_and_legacy_taints_when_ready() {
        let current = [
            taint("CriticalAddonsOnly", None, "NoSchedule"),
            unreachable(),
            our_taint(DEFAULT_TAINT_KEY),
        ];
        let new = plan_taints(&current, false, DEFAULT_TAINT_KEY)
            .new_taints
            .unwrap();
        assert_eq!(new, vec![unreachable()]);
    }

    #[test]
    fn replaces_legacy_taint_when_not_ready() {
        let current = [taint("CriticalAddonsOnly", None, "NoSchedule")];
        let new = plan_taints(&current, true, DEFAULT_TAINT_KEY)
            .new_taints
            .unwrap();
        assert_eq!(new, vec![our_taint(DEFAULT_TAINT_KEY)]);
    }

    #[test]
    fn never_touches_aks_system_taint() {
        let current = [aks_system()];
        assert!(plan_taints(&current, false, DEFAULT_TAINT_KEY)
            .new_taints
            .is_none());
        let new = plan_taints(&current, true, DEFAULT_TAINT_KEY)
            .new_taints
            .unwrap();
        assert_eq!(new, vec![aks_system(), our_taint(DEFAULT_TAINT_KEY)]);
    }

    #[test]
    fn foreign_taint_on_same_key_blocks_ours() {
        let plan = plan_taints(&[aks_system()], true, "CriticalAddonsOnly");
        assert!(plan.new_taints.is_none());
        assert_eq!(plan.blocked_by, Some(aks_system()));
    }

    #[test]
    fn normalizes_valueless_own_key() {
        let current = [taint(DEFAULT_TAINT_KEY, None, "NoSchedule")];
        let new = plan_taints(&current, true, DEFAULT_TAINT_KEY)
            .new_taints
            .unwrap();
        assert_eq!(new, vec![our_taint(DEFAULT_TAINT_KEY)]);
    }

    fn pod(phase: &str, ready: &str, terminating: bool) -> Pod {
        let mut pod = Pod {
            status: Some(PodStatus {
                phase: Some(phase.into()),
                conditions: Some(vec![PodCondition {
                    type_: "Ready".into(),
                    status: ready.into(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        if terminating {
            pod.metadata.deletion_timestamp = Some(Time(Default::default()));
        }
        pod
    }

    #[test]
    fn readiness_requires_running_ready_and_not_terminating() {
        assert!(is_multus_ready(&pod("Running", "True", false)));
        assert!(!is_multus_ready(&pod("Running", "False", false)));
        assert!(!is_multus_ready(&pod("Pending", "True", false)));
        assert!(!is_multus_ready(&pod("Running", "True", true)));
        assert!(!is_multus_ready(&Pod::default()));
    }
}
