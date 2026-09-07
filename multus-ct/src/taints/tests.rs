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
    let new = plan_taints(&[unreachable()], true, DEFAULT_TAINT_KEY);
    assert_eq!(new, Some(vec![unreachable(), our_taint(DEFAULT_TAINT_KEY)]));
}

#[test]
fn noop_when_present_regardless_of_order() {
    let current = [our_taint(DEFAULT_TAINT_KEY), unreachable()];
    assert_eq!(plan_taints(&current, true, DEFAULT_TAINT_KEY), None);
}

#[test]
fn noop_when_absent_and_ready() {
    assert_eq!(
        plan_taints(&[unreachable()], false, DEFAULT_TAINT_KEY),
        None
    );
}

#[test]
fn removes_only_own_taint_when_ready() {
    let current = [aks_system(), unreachable(), our_taint(DEFAULT_TAINT_KEY)];
    let new = plan_taints(&current, false, DEFAULT_TAINT_KEY);
    assert_eq!(new, Some(vec![aks_system(), unreachable()]));
}

#[test]
fn never_touches_aks_system_taint() {
    let current = [aks_system()];
    assert_eq!(plan_taints(&current, false, DEFAULT_TAINT_KEY), None);
    let new = plan_taints(&current, true, DEFAULT_TAINT_KEY);
    assert_eq!(new, Some(vec![aks_system(), our_taint(DEFAULT_TAINT_KEY)]));
}

#[test]
fn foreign_taint_on_same_key_is_left_alone() {
    assert_eq!(
        plan_taints(&[aks_system()], true, "CriticalAddonsOnly"),
        None
    );
    assert_eq!(
        plan_taints(&[aks_system()], false, "CriticalAddonsOnly"),
        None
    );
}

#[test]
fn normalizes_valueless_own_key() {
    let current = [taint(DEFAULT_TAINT_KEY, None, "NoSchedule")];
    let new = plan_taints(&current, true, DEFAULT_TAINT_KEY);
    assert_eq!(new, Some(vec![our_taint(DEFAULT_TAINT_KEY)]));
}

#[test]
fn node_taints_defaults_to_empty() {
    assert!(node_taints(&Node::default()).is_empty());
    assert!(!has_managed_taint(&Node::default(), DEFAULT_TAINT_KEY));
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
