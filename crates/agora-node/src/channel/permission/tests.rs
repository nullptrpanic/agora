use super::*;
use crate::config::{
    ChannelGroupPermissionConfig, ChannelPermissionConfig, ChannelUserPermissionConfig,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn policy(users: &[&str], groups: &[(&str, bool)]) -> PermissionGate {
    PermissionGate::new(ChannelPermissionConfig {
        users: users
            .iter()
            .map(|id| ChannelUserPermissionConfig {
                id: (*id).to_string(),
            })
            .collect(),
        groups: groups
            .iter()
            .map(|(id, require_mention)| ChannelGroupPermissionConfig {
                id: (*id).to_string(),
                require_mention: *require_mention,
            })
            .collect(),
    })
}

#[test]
fn empty_permission_denies_private_and_group_access() {
    let permission = policy(&[], &[]);

    assert_eq!(
        permission.authorize(&AccessContext::private("user-1")),
        Err(DenialReason::UserNotAllowed)
    );
    assert_eq!(
        permission.authorize(&AccessContext::group("user-1", "group-1", true)),
        Err(DenialReason::UserNotAllowed)
    );
}

#[test]
fn private_access_accepts_an_explicit_user_or_user_wildcard() {
    assert_eq!(
        policy(&["user-1"], &[]).authorize(&AccessContext::private("user-1")),
        Ok(())
    );
    assert_eq!(
        policy(&["*"], &[]).authorize(&AccessContext::private("any-user")),
        Ok(())
    );
}

#[test]
fn group_access_requires_both_user_and_group_permission() {
    let permission = policy(&["user-1"], &[("group-1", false)]);

    assert_eq!(
        permission.authorize(&AccessContext::group("user-2", "group-1", false)),
        Err(DenialReason::UserNotAllowed)
    );
    assert_eq!(
        permission.authorize(&AccessContext::group("user-1", "group-2", false)),
        Err(DenialReason::GroupNotAllowed)
    );
    assert_eq!(
        permission.authorize(&AccessContext::group("user-1", "group-1", false)),
        Ok(())
    );
}

#[test]
fn exact_group_rule_overrides_the_wildcard_mention_requirement() {
    let permission = policy(&["*"], &[("*", true), ("group-1", false)]);

    assert_eq!(
        permission.authorize(&AccessContext::group("user-1", "group-1", false)),
        Ok(())
    );
    assert_eq!(
        permission.authorize(&AccessContext::group("user-1", "group-2", false)),
        Err(DenialReason::MentionRequired)
    );
    assert_eq!(
        permission.authorize(&AccessContext::group("user-1", "group-2", true)),
        Ok(())
    );
}

#[test]
fn structured_actions_skip_only_the_mention_requirement() {
    let permission = policy(&["user-1"], &[("group-1", true)]);

    assert_eq!(
        permission.authorize(&AccessContext::group_action("user-1", "group-1")),
        Ok(())
    );
    assert_eq!(
        permission.authorize(&AccessContext::group_action("user-2", "group-1")),
        Err(DenialReason::UserNotAllowed)
    );
}

#[test]
fn unresolved_group_context_is_denied_even_with_wildcards() {
    let permission = policy(&["*"], &[("*", false)]);

    assert_eq!(
        permission.authorize(&AccessContext::unresolved_group("user-1", "group-1")),
        Err(DenialReason::GroupNotAllowed)
    );
}

#[tokio::test]
async fn permission_gate_delivers_denial_and_consumes_the_event() {
    let permission = policy(&["user-1"], &[]);
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&delivered);

    let admitted = permission
        .admit(
            "channel-1",
            &AccessContext::private("user-2"),
            move |message| async move {
                captured.lock().unwrap().push(message);
                Ok(())
            },
        )
        .await;

    assert!(!admitted);
    let delivered = delivered.lock().unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].channel_name(), "channel-1");
    assert_eq!(delivered[0].user_id(), "user-2");
    assert_eq!(delivered[0].group_id(), None);
    assert_eq!(delivered[0].reason(), "当前用户未在允许列表中。");
    let configuration = delivered[0].configuration_example();
    assert!(configuration.contains(r#""channels": ["#));
    assert!(configuration.contains("// ..."));
    assert!(configuration.contains(r#""permission": {"#));
    assert!(configuration.contains(r#""id": "user-2""#));
    assert!(!configuration.contains(r#""name": "channel-1""#));
    assert!(!configuration.contains("```"));
}

#[tokio::test]
async fn permission_gate_does_not_call_the_denial_delivery_when_allowed() {
    let permission = policy(&["user-1"], &[]);
    let delivered = Arc::new(AtomicBool::new(false));
    let captured = Arc::clone(&delivered);

    let admitted = permission
        .admit(
            "channel-1",
            &AccessContext::private("user-1"),
            move |_| async move {
                captured.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

    assert!(admitted);
    assert!(!delivered.load(Ordering::Relaxed));
}

#[tokio::test]
async fn permission_gate_silently_consumes_unmentioned_denied_group_messages() {
    let permission = policy(&["user-1"], &[("group-1", false)]);
    let delivered = Arc::new(AtomicBool::new(false));
    let captured = Arc::clone(&delivered);

    let admitted = permission
        .admit(
            "channel-1",
            &AccessContext::group("user-2", "group-1", false),
            move |_| async move {
                captured.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

    assert!(!admitted);
    assert!(!delivered.load(Ordering::Relaxed));
}

#[tokio::test]
async fn permission_gate_reports_group_denial_even_when_delivery_fails() {
    let permission = policy(&["user-1"], &[]);
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&delivered);

    let admitted = permission
        .admit(
            "channel-1",
            &AccessContext::group("user-1", "group-1", true),
            move |message| async move {
                captured.lock().unwrap().push(message);
                Err::<(), _>(anyhow::anyhow!("delivery failed"))
            },
        )
        .await;

    assert!(!admitted);
    let delivered = delivered.lock().unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].reason(), "当前群聊未在允许列表中。");
}
