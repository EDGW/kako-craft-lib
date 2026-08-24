mod compare;
mod path;
pub(crate) mod report;

pub(crate) use compare::validate_link_snapshots;

use self::path::{is_transient_error, resolved_recorded_path};
use self::report::{merge_validation_report, push_issue};
use super::{
    ContainerLinkSnapshot, EntryKey, LinkValidationIssueKind, LinkValidationReport,
    LinkValidationRunError, LinkValidationUnavailable, open_container,
};

pub(crate) fn validate_recorded_link_snapshots(
    current: &ContainerLinkSnapshot,
    key: &EntryKey,
) -> std::result::Result<LinkValidationReport, LinkValidationRunError> {
    let outgoing: Vec<_> = current
        .outgoing
        .iter()
        .filter(|record| record.linker_key == *key)
        .collect();
    if outgoing.is_empty() {
        return Ok(LinkValidationReport::default());
    }

    let mut report = LinkValidationReport::default();
    let mut containers = Vec::new();
    for record in outgoing {
        let path = resolved_recorded_path(current, record);
        let container = match open_container(&path) {
            Ok(container) => container,
            Err(error) if is_transient_error(&error) => {
                report.unavailable.push(LinkValidationUnavailable {
                    container_uid: record.target_container_uid.clone(),
                    container_path: path,
                    reason: error.to_string(),
                });
                continue;
            }
            Err(error) => {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::ContainerPathMissing,
                    current,
                    key,
                    &record.target_container_uid,
                    &path,
                    Some(&record.linker_key),
                    Some(&record.target_key),
                    Some(&record.target_container_uid),
                    Some(&error.to_string()),
                );
                continue;
            }
        };
        let actual_uid = container.uid()?;
        if actual_uid != record.target_container_uid {
            push_issue(
                &mut report,
                LinkValidationIssueKind::ContainerPathUidMismatch,
                current,
                key,
                &actual_uid,
                &path,
                Some(&record.linker_key),
                Some(&record.target_key),
                Some(&record.target_container_uid),
                Some(&actual_uid),
            );
            continue;
        }
        containers.push(container);
    }
    if !containers.is_empty() {
        let refs = containers
            .iter()
            .map(|container| container.as_ref())
            .collect::<Vec<_>>();
        merge_validation_report(&mut report, validate_link_snapshots(current, key, &refs)?);
    }
    Ok(report)
}
