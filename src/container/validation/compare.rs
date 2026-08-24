use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::super::{
    Container, ContainerLinkSnapshot, EntryKey, LinkValidationIssueKind, LinkValidationReport,
    LinkValidationRunError, LinkValidationUnavailable,
};
use super::path::{
    is_unavailable_snapshot_error, normalized_path, recorded_path_uid,
    validate_materialized_symlink, validate_recorded_container_path,
};
use super::report::{detect_duplicate_incoming, push_issue, push_match};

struct CorrespondingSnapshot {
    uid: String,
    path: PathBuf,
    snapshot: Option<ContainerLinkSnapshot>,
}

pub(crate) fn validate_link_snapshots(
    current: &ContainerLinkSnapshot,
    key: &EntryKey,
    corresponding: &[&dyn Container],
) -> std::result::Result<LinkValidationReport, LinkValidationRunError> {
    let current_path = normalized_path(&current.container_path)?;
    let mut report = LinkValidationReport::default();
    let mut seen_uid_paths: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut snapshots = Vec::new();
    let mut inputs = Vec::new();

    for container in corresponding {
        let uid = container.uid()?;
        let path = normalized_path(&container.root_path())?;
        if uid == current.container_uid {
            return Err(LinkValidationRunError::SelfValidation(uid));
        }
        if let Some(previous) = seen_uid_paths.get(&uid) {
            if previous == &path {
                continue;
            }
            return Err(LinkValidationRunError::DuplicateContainerUid { uid });
        }
        seen_uid_paths.insert(uid.clone(), path.clone());
        inputs.push((*container, uid, path));
    }
    inputs.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.2.cmp(&right.2)));

    for (container, uid, path) in inputs {
        match container.writer() {
            Ok(guard) => match guard.link_snapshot() {
                Ok(snapshot) => snapshots.push(CorrespondingSnapshot {
                    uid,
                    path,
                    snapshot: Some(snapshot),
                }),
                Err(error) => {
                    let referenced = current
                        .incoming
                        .iter()
                        .any(|record| record.target_key == *key && record.linker_uid == uid)
                        || current.outgoing.iter().any(|record| {
                            record.linker_key == *key && record.target_container_uid == uid
                        });
                    if referenced && !is_unavailable_snapshot_error(&error) {
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::MetadataInvalid,
                            current,
                            key,
                            &uid,
                            &path,
                            None,
                            None,
                            Some("valid link metadata"),
                            Some(&error.to_string()),
                        );
                    } else {
                        report.unavailable.push(LinkValidationUnavailable {
                            container_uid: uid.clone(),
                            container_path: path.clone(),
                            reason: error.to_string(),
                        });
                    }
                    snapshots.push(CorrespondingSnapshot {
                        uid,
                        path,
                        snapshot: None,
                    });
                }
            },
            Err(error) => {
                report.unavailable.push(LinkValidationUnavailable {
                    container_uid: uid.clone(),
                    container_path: path.clone(),
                    reason: error.to_string(),
                });
                snapshots.push(CorrespondingSnapshot {
                    uid,
                    path,
                    snapshot: None,
                });
            }
        }
    }

    let supplied_uids: BTreeSet<_> = snapshots.iter().map(|item| item.uid.as_str()).collect();
    let current_incoming: Vec<_> = current
        .incoming
        .iter()
        .filter(|record| record.target_key == *key)
        .collect();
    let current_outgoing: Vec<_> = current
        .outgoing
        .iter()
        .filter(|record| record.linker_key == *key)
        .collect();

    report.ignored_current_records += current_incoming
        .iter()
        .filter(|record| !supplied_uids.contains(record.linker_uid.as_str()))
        .count();
    report.ignored_current_records += current_outgoing
        .iter()
        .filter(|record| !supplied_uids.contains(record.target_container_uid.as_str()))
        .count();

    let mut valid_seen = BTreeSet::new();
    for corresponding in snapshots {
        let current_refs_this_uid = current_incoming
            .iter()
            .any(|record| record.linker_uid == corresponding.uid)
            || current_outgoing
                .iter()
                .any(|record| record.target_container_uid == corresponding.uid);

        let Some(other) = corresponding.snapshot.as_ref() else {
            if !current_refs_this_uid {
                // We cannot prove that a locked container is unrelated.
                continue;
            }
            continue;
        };

        let mut relevant_incoming = BTreeSet::new();
        let mut relevant_outgoing = BTreeSet::new();

        let incoming_from_this_container: Vec<_> = current_incoming
            .iter()
            .copied()
            .filter(|record| record.linker_uid == corresponding.uid)
            .collect();
        detect_duplicate_incoming(
            current,
            key,
            &corresponding.uid,
            &corresponding.path,
            &incoming_from_this_container,
            &mut report,
        );

        let corresponding_incoming_candidates: Vec<_> = other
            .incoming
            .iter()
            .filter(|record| {
                record.linker_uid == current.container_uid && record.linker_key == *key
            })
            .collect();
        detect_duplicate_incoming(
            current,
            key,
            &corresponding.uid,
            &corresponding.path,
            &corresponding_incoming_candidates,
            &mut report,
        );

        // Validate every incoming record in the current container which claims
        // to originate from this corresponding container.
        for incoming in current_incoming
            .iter()
            .filter(|record| record.linker_uid == corresponding.uid)
        {
            let candidates: Vec<_> = other
                .outgoing
                .iter()
                .enumerate()
                .filter(|(_, record)| record.linker_key == incoming.linker_key)
                .collect();
            if candidates.is_empty() {
                let mismatched_keys = other
                    .outgoing
                    .iter()
                    .enumerate()
                    .filter(|(_, outgoing)| {
                        outgoing.target_key == *key
                            && (outgoing.target_container_uid == current.container_uid
                                || recorded_path_uid(other, outgoing)
                                    .is_some_and(|uid| uid == current.container_uid))
                    })
                    .collect::<Vec<_>>();
                if mismatched_keys.is_empty() {
                    push_issue(
                        &mut report,
                        LinkValidationIssueKind::MissingOutgoingRecord,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(&incoming.linker_key),
                        Some(key),
                        Some("matching outgoing record"),
                        None,
                    );
                } else {
                    for (index, outgoing) in mismatched_keys {
                        relevant_outgoing.insert(index);
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::LinkerKeyMismatch,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(&outgoing.linker_key),
                            Some(&outgoing.target_key),
                            Some(&incoming.linker_key),
                            Some(&outgoing.linker_key),
                        );
                        validate_recorded_container_path(
                            other,
                            outgoing,
                            &current.container_uid,
                            &current_path,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            &mut report,
                        );
                        validate_materialized_symlink(
                            other,
                            outgoing,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            &mut report,
                        );
                    }
                }
                continue;
            }
            if candidates.len() > 1 {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::DuplicateOutgoingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&incoming.linker_key),
                    Some(key),
                    Some("one outgoing record"),
                    Some(&candidates.len().to_string()),
                );
            }
            for (index, outgoing) in candidates {
                relevant_outgoing.insert(index);
                let mut matches = true;
                if outgoing.target_container_uid != current.container_uid {
                    matches = false;
                    push_issue(
                        &mut report,
                        LinkValidationIssueKind::TargetUidMismatch,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(&outgoing.linker_key),
                        Some(&outgoing.target_key),
                        Some(&current.container_uid),
                        Some(&outgoing.target_container_uid),
                    );
                }
                if outgoing.target_key != *key {
                    matches = false;
                    push_issue(
                        &mut report,
                        LinkValidationIssueKind::TargetKeyMismatch,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(&outgoing.linker_key),
                        Some(&outgoing.target_key),
                        Some(key),
                        Some(&outgoing.target_key),
                    );
                }
                if !validate_recorded_container_path(
                    other,
                    outgoing,
                    &current.container_uid,
                    &current_path,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    &mut report,
                ) {
                    matches = false;
                }
                if !validate_materialized_symlink(
                    other,
                    outgoing,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    &mut report,
                ) {
                    matches = false;
                }
                if matches {
                    push_match(
                        &mut report,
                        &mut valid_seen,
                        &corresponding.uid,
                        &outgoing.linker_key,
                        &current.container_uid,
                        key,
                    );
                }
            }
        }

        // Find outgoing records which independently claim this current entry,
        // including records whose stored UID is wrong but whose path resolves
        // to the current container.
        for (index, outgoing) in other.outgoing.iter().enumerate() {
            if relevant_outgoing.contains(&index) {
                continue;
            }
            let same_key_claimed = current_incoming
                .iter()
                .any(|record| record.linker_key == outgoing.linker_key);
            let uid_claim = outgoing.target_container_uid == current.container_uid
                && outgoing.target_key == *key;
            let path_claim = outgoing.target_key == *key
                && recorded_path_uid(other, outgoing)
                    .is_some_and(|uid| uid == current.container_uid);
            if !(same_key_claimed || uid_claim || path_claim) {
                continue;
            }
            relevant_outgoing.insert(index);
            if outgoing.target_container_uid != current.container_uid {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::TargetUidMismatch,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&outgoing.linker_key),
                    Some(&outgoing.target_key),
                    Some(&current.container_uid),
                    Some(&outgoing.target_container_uid),
                );
            }
            if outgoing.target_key != *key {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::TargetKeyMismatch,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&outgoing.linker_key),
                    Some(&outgoing.target_key),
                    Some(key),
                    Some(&outgoing.target_key),
                );
            }
            validate_recorded_container_path(
                other,
                outgoing,
                &current.container_uid,
                &current_path,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            validate_materialized_symlink(
                other,
                outgoing,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            let claimed_by: Vec<_> = current_incoming
                .iter()
                .filter(|record| record.linker_key == outgoing.linker_key)
                .collect();
            if !claimed_by.is_empty() {
                for record in claimed_by {
                    if record.linker_uid == corresponding.uid {
                        // An exact-UID record would already have consumed this
                        // outgoing key above. Reaching here means it is a duplicate.
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::DuplicateOutgoingRecord,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(&outgoing.linker_key),
                            Some(&outgoing.target_key),
                            None,
                            None,
                        );
                    } else {
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::LinkerUidMismatch,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(&outgoing.linker_key),
                            Some(&outgoing.target_key),
                            Some(&record.linker_uid),
                            Some(&corresponding.uid),
                        );
                    }
                }
            } else {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::UnexpectedOutgoingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(&outgoing.linker_key),
                    Some(&outgoing.target_key),
                    None,
                    Some("outgoing record has no matching incoming record"),
                );
            }
        }

        // Validate the current entry's outgoing record against all incoming
        // records in the corresponding container.
        for outgoing in current_outgoing
            .iter()
            .filter(|record| record.target_container_uid == corresponding.uid)
        {
            let mut exact = false;
            for (index, incoming) in other.incoming.iter().enumerate() {
                if incoming.linker_uid == current.container_uid && incoming.linker_key == *key {
                    relevant_incoming.insert(index);
                    if incoming.target_key == outgoing.target_key {
                        exact = true;
                    } else {
                        push_issue(
                            &mut report,
                            LinkValidationIssueKind::TargetKeyMismatch,
                            current,
                            key,
                            &corresponding.uid,
                            &corresponding.path,
                            Some(key),
                            Some(&incoming.target_key),
                            Some(&outgoing.target_key),
                            Some(&incoming.target_key),
                        );
                    }
                }
            }
            let path_valid = validate_recorded_container_path(
                current,
                outgoing,
                &corresponding.uid,
                &corresponding.path,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            let symlink_valid = validate_materialized_symlink(
                current,
                outgoing,
                current,
                key,
                &corresponding.uid,
                &corresponding.path,
                &mut report,
            );
            if !exact {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::MissingIncomingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(key),
                    Some(&outgoing.target_key),
                    Some("matching incoming record"),
                    None,
                );
            } else if path_valid && symlink_valid {
                push_match(
                    &mut report,
                    &mut valid_seen,
                    &current.container_uid,
                    key,
                    &corresponding.uid,
                    &outgoing.target_key,
                );
            }
        }

        // Incoming records in the corresponding container which independently
        // claim the current linker entry must also have a matching current
        // outgoing record.
        for (index, incoming) in other.incoming.iter().enumerate() {
            if relevant_incoming.contains(&index)
                || incoming.linker_uid != current.container_uid
                || incoming.linker_key != *key
            {
                continue;
            }
            relevant_incoming.insert(index);
            if !current_outgoing.is_empty() {
                for outgoing in &current_outgoing {
                    let kind = if outgoing.target_container_uid != corresponding.uid {
                        LinkValidationIssueKind::TargetUidMismatch
                    } else {
                        LinkValidationIssueKind::TargetKeyMismatch
                    };
                    push_issue(
                        &mut report,
                        kind,
                        current,
                        key,
                        &corresponding.uid,
                        &corresponding.path,
                        Some(key),
                        Some(&incoming.target_key),
                        Some(&format!(
                            "{}:{}",
                            outgoing.target_container_uid, outgoing.target_key
                        )),
                        Some(&format!("{}:{}", corresponding.uid, incoming.target_key)),
                    );
                }
            } else {
                push_issue(
                    &mut report,
                    LinkValidationIssueKind::UnexpectedIncomingRecord,
                    current,
                    key,
                    &corresponding.uid,
                    &corresponding.path,
                    Some(key),
                    Some(&incoming.target_key),
                    None,
                    Some("incoming record has no matching outgoing record"),
                );
            }
        }

        report.ignored_corresponding_records +=
            other.incoming.len().saturating_sub(relevant_incoming.len());
        report.ignored_corresponding_records +=
            other.outgoing.len().saturating_sub(relevant_outgoing.len());
        if relevant_incoming.is_empty() && relevant_outgoing.is_empty() && !current_refs_this_uid {
            report.ignored_containers += 1;
        }
    }

    Ok(report)
}
