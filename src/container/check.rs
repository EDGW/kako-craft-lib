use std::collections::BTreeSet;

use anyhow::Result;

use super::{
    CheckActionError, CheckActionResult, CheckRepairAction, Container, ContainerWriteGuard,
    LinkCheckIssue, LinkCheckKind, LinkValidationIssueKind, open_container,
};

pub(crate) fn validation_check_issues<G: ContainerWriteGuard + ?Sized>(
    guard: &G,
    corresponding: &[&dyn Container],
) -> Result<Vec<LinkCheckIssue>> {
    let snapshot = guard.link_snapshot()?;
    let keys = snapshot
        .incoming
        .iter()
        .map(|record| record.target_key.clone())
        .chain(
            snapshot
                .outgoing
                .iter()
                .map(|record| record.linker_key.clone()),
        )
        .collect::<BTreeSet<_>>();
    let mut issues = Vec::new();
    for key in keys {
        let report = guard.validate_links(&key, corresponding)?;
        let current_is_outgoing = snapshot
            .outgoing
            .iter()
            .any(|record| record.linker_key == key);
        for issue in report.broken {
            if matches!(
                issue.kind,
                LinkValidationIssueKind::MaterializedSymlinkMissing
                    | LinkValidationIssueKind::MaterializedSymlinkMismatch
            ) {
                continue;
            }
            let corresponding_is_link = corresponding.iter().any(|container| {
                container
                    .uid()
                    .is_ok_and(|uid| uid == issue.corresponding_container_uid)
                    && container.kind() == "link"
            });
            let actions = match issue.kind {
                LinkValidationIssueKind::MissingOutgoingRecord if corresponding_is_link => vec![
                    CheckRepairAction::AddMissingOutgoingRecord,
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::MissingOutgoingRecord => vec![
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::MissingIncomingRecord if current_is_outgoing => vec![
                    CheckRepairAction::AddMissingIncomingRecord,
                    CheckRepairAction::RemoveStaleOutgoingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::UnexpectedOutgoingRecord => vec![
                    CheckRepairAction::AddMissingIncomingRecord,
                    CheckRepairAction::RemoveStaleOutgoingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::UnexpectedIncomingRecord => vec![
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::TargetUidMismatch
                | LinkValidationIssueKind::TargetKeyMismatch => vec![
                    CheckRepairAction::RemoveStaleIncomingRecord,
                    CheckRepairAction::Skip,
                ],
                LinkValidationIssueKind::ContainerPathMissing
                | LinkValidationIssueKind::ContainerPathMismatch
                | LinkValidationIssueKind::ContainerPathUidMismatch
                | LinkValidationIssueKind::TargetEntryMissing
                    if current_is_outgoing =>
                {
                    vec![
                        CheckRepairAction::RemoveLocalOutgoingOnly,
                        CheckRepairAction::Skip,
                    ]
                }
                _ => vec![CheckRepairAction::Skip],
            };
            issues.push(LinkCheckIssue {
                id: format!(
                    "validation:{:?}:{}:{}:{}:{}",
                    issue.kind,
                    issue.current_key,
                    issue.corresponding_container_uid,
                    issue.linker_key.as_deref().unwrap_or("-"),
                    issue.target_key.as_deref().unwrap_or("-")
                ),
                key: issue.current_key,
                kind: LinkCheckKind::Validation(issue.kind),
                corresponding_container_uid: Some(issue.corresponding_container_uid),
                corresponding_container_path: Some(issue.corresponding_container_path),
                linker_key: issue.linker_key,
                target_key: issue.target_key,
                expected: issue.expected,
                actual: issue.actual,
                actions,
            });
        }
        for unavailable in report.unavailable {
            issues.push(LinkCheckIssue {
                id: format!(
                    "unavailable:{}:{}:{}",
                    key,
                    unavailable.container_uid,
                    unavailable.container_path.display()
                ),
                key: key.clone(),
                kind: LinkCheckKind::Unavailable,
                corresponding_container_uid: Some(unavailable.container_uid),
                corresponding_container_path: Some(unavailable.container_path),
                linker_key: None,
                target_key: None,
                expected: None,
                actual: Some(unavailable.reason),
                actions: vec![CheckRepairAction::RetryUnavailable, CheckRepairAction::Skip],
            });
        }
    }
    issues.sort_by(|left, right| left.id.cmp(&right.id));
    issues.dedup_by(|left, right| left.id == right.id);
    Ok(issues)
}

pub(crate) fn apply_validation_check_action<G: ContainerWriteGuard + ?Sized>(
    guard: &mut G,
    issue: &LinkCheckIssue,
    action: CheckRepairAction,
    corresponding: &[&dyn Container],
) -> Result<CheckActionResult> {
    if action == CheckRepairAction::Skip {
        return Ok(CheckActionResult {
            description: format!("skipped issue {}", issue.id),
        });
    }
    if issue.kind == LinkCheckKind::Unavailable && action == CheckRepairAction::RetryUnavailable {
        return Ok(CheckActionResult {
            description: format!("retry requested for {}", issue.id),
        });
    }
    let current_uid = guard.container_uid().to_owned();
    let corresponding_uid = issue
        .corresponding_container_uid
        .as_deref()
        .ok_or_else(|| CheckActionError::MissingContext {
            issue_id: issue.id.clone(),
            field: "corresponding container UID",
        })?;
    let linker_key = issue
        .linker_key
        .as_ref()
        .ok_or_else(|| CheckActionError::MissingContext {
            issue_id: issue.id.clone(),
            field: "linker key",
        })?;
    let target_key = issue
        .target_key
        .as_ref()
        .ok_or_else(|| CheckActionError::MissingContext {
            issue_id: issue.id.clone(),
            field: "target key",
        })?;

    match (issue.kind.clone(), action) {
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingOutgoingRecord),
            CheckRepairAction::AddMissingOutgoingRecord,
        ) => {
            let current = guard.link_snapshot()?;
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.repair_add_outgoing(
                linker_key,
                &current.container_uid,
                &current.container_path,
                &issue.key,
            )?;
            Ok(CheckActionResult {
                description: format!(
                    "created missing outgoing record {}:{} -> {}:{}",
                    corresponding_uid, linker_key, current.container_uid, issue.key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingOutgoingRecord),
            CheckRepairAction::RemoveStaleIncomingRecord,
        ) => {
            guard.unlink(corresponding_uid, &issue.key, linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "removed stale incoming record {}:{} from {}",
                    corresponding_uid, linker_key, issue.key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::UnexpectedOutgoingRecord),
            CheckRepairAction::AddMissingIncomingRecord,
        ) => {
            guard.link_from(corresponding_uid, &issue.key, linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "added incoming record {}:{} to {}",
                    corresponding_uid, linker_key, issue.key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingIncomingRecord),
            CheckRepairAction::AddMissingIncomingRecord,
        ) => {
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.link_from(&current_uid, target_key, linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "added reciprocal incoming record {}:{} to {}:{}",
                    current_uid, linker_key, corresponding_uid, target_key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::MissingIncomingRecord),
            CheckRepairAction::RemoveStaleOutgoingRecord,
        ) => {
            guard.repair_remove_outgoing(&issue.key)?;
            Ok(CheckActionResult {
                description: format!("removed stale outgoing record and symlink {}", issue.key),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::UnexpectedOutgoingRecord),
            CheckRepairAction::RemoveStaleOutgoingRecord,
        ) => {
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.repair_remove_outgoing(linker_key)?;
            Ok(CheckActionResult {
                description: format!(
                    "removed stale outgoing record and symlink {}:{}",
                    corresponding_uid, linker_key
                ),
            })
        }
        (
            LinkCheckKind::Validation(LinkValidationIssueKind::UnexpectedIncomingRecord),
            CheckRepairAction::RemoveStaleIncomingRecord,
        ) => {
            let mut writer = issue_corresponding_writer(issue, corresponding)?;
            writer.unlink(&current_uid, target_key, &issue.key)?;
            Ok(CheckActionResult {
                description: format!(
                    "removed stale incoming record {}:{} from {}:{}",
                    current_uid, issue.key, corresponding_uid, target_key
                ),
            })
        }
        (
            LinkCheckKind::Validation(
                LinkValidationIssueKind::TargetUidMismatch
                | LinkValidationIssueKind::TargetKeyMismatch,
            ),
            CheckRepairAction::RemoveStaleIncomingRecord,
        ) => {
            let current = guard.link_snapshot()?;
            if current
                .outgoing
                .iter()
                .any(|record| record.linker_key == issue.key)
            {
                let mut writer = issue_corresponding_writer(issue, corresponding)?;
                writer.unlink(&current_uid, target_key, &issue.key)?;
            } else {
                guard.unlink(corresponding_uid, &issue.key, linker_key)?;
            }
            Ok(CheckActionResult {
                description: format!(
                    "removed mismatched incoming record for {}:{}",
                    corresponding_uid, linker_key
                ),
            })
        }
        _ => Err(CheckActionError::InvalidAction {
            issue_id: issue.id.clone(),
            action,
        }
        .into()),
    }
}

fn corresponding_by_uid<'a>(
    corresponding: &'a [&dyn Container],
    uid: &str,
) -> Result<Option<&'a dyn Container>> {
    let mut matched = None;
    for container in corresponding {
        if container.uid()? == uid {
            if matched.is_some() {
                return Err(CheckActionError::AmbiguousContainerUid(uid.to_owned()).into());
            }
            matched = Some(*container);
        }
    }
    Ok(matched)
}

fn issue_corresponding_writer(
    issue: &LinkCheckIssue,
    corresponding: &[&dyn Container],
) -> Result<Box<dyn ContainerWriteGuard>> {
    let uid = issue
        .corresponding_container_uid
        .as_deref()
        .ok_or_else(|| CheckActionError::MissingContext {
            issue_id: issue.id.clone(),
            field: "corresponding container UID",
        })?;
    match corresponding_by_uid(corresponding, uid)? {
        Some(container) => container.writer().map_err(Into::into),
        None => {
            let path = issue.corresponding_container_path.as_ref().ok_or_else(|| {
                CheckActionError::MissingContext {
                    issue_id: issue.id.clone(),
                    field: "corresponding container path",
                }
            })?;
            let container = open_container(path)?;
            let actual_uid = container.uid()?;
            if actual_uid != uid {
                return Err(CheckActionError::ContainerUidMismatch {
                    path: path.clone(),
                    expected: uid.to_owned(),
                    actual: actual_uid,
                }
                .into());
            }
            container.writer().map_err(Into::into)
        }
    }
}
