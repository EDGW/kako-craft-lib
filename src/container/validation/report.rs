//! Validation-report construction, deduplication, ordering, and logging.

use std::collections::BTreeSet;
use std::path::Path;

use super::super::{
    ContainerLinkSnapshot, EntryKey, IncomingLinkRecord, LinkMatch, LinkValidationIssue,
    LinkValidationIssueKind, LinkValidationReport,
};

/// Appends every result category and ignored counter from one report into another.
///
/// # Arguments
///
/// * `into` - Destination report retained and mutated.
/// * `other` - Source report consumed so its vectors can be appended without cloning.
///
/// # Returns
///
/// Returns after all vectors are appended and each ignored counter is added to `into`.
pub(crate) fn merge_validation_report(
    into: &mut LinkValidationReport,
    mut other: LinkValidationReport,
) {
    into.valid.append(&mut other.valid);
    into.broken.append(&mut other.broken);
    into.unavailable.append(&mut other.unavailable);
    into.ignored_current_records += other.ignored_current_records;
    into.ignored_corresponding_records += other.ignored_corresponding_records;
    into.ignored_containers += other.ignored_containers;
}

/// Sorts every validation result category into deterministic presentation order.
///
/// # Arguments
///
/// * `report` - Report whose valid, broken, and unavailable vectors are reordered in place.
///
/// # Returns
///
/// Returns after valid identities, ranked issues, and unavailable containers are sorted.
pub(crate) fn sort_validation_report(report: &mut LinkValidationReport) {
    report.valid.sort_by(|left, right| {
        (
            &left.linker_container_uid,
            &left.linker_key,
            &left.target_container_uid,
            &left.target_key,
        )
            .cmp(&(
                &right.linker_container_uid,
                &right.linker_key,
                &right.target_container_uid,
                &right.target_key,
            ))
    });
    report.broken.sort_by(|left, right| {
        (
            validation_issue_rank(left.kind),
            &left.corresponding_container_uid,
            &left.current_key,
            &left.linker_key,
            &left.target_key,
        )
            .cmp(&(
                validation_issue_rank(right.kind),
                &right.corresponding_container_uid,
                &right.current_key,
                &right.linker_key,
                &right.target_key,
            ))
    });
    report.unavailable.sort_by(|left, right| {
        (&left.container_uid, &left.container_path)
            .cmp(&(&right.container_uid, &right.container_path))
    });
}

/// Maps issue kinds to their stable report-order priority.
///
/// # Arguments
///
/// * `kind` - Validation issue category to rank.
///
/// # Returns
///
/// A lower number for issues displayed earlier in a sorted report.
fn validation_issue_rank(kind: LinkValidationIssueKind) -> u8 {
    match kind {
        LinkValidationIssueKind::MissingOutgoingRecord => 0,
        LinkValidationIssueKind::UnexpectedOutgoingRecord => 1,
        LinkValidationIssueKind::MissingIncomingRecord => 2,
        LinkValidationIssueKind::UnexpectedIncomingRecord => 3,
        LinkValidationIssueKind::LinkerUidMismatch => 4,
        LinkValidationIssueKind::TargetUidMismatch => 5,
        LinkValidationIssueKind::LinkerKeyMismatch => 6,
        LinkValidationIssueKind::TargetKeyMismatch => 7,
        LinkValidationIssueKind::TargetEntryMissing => 8,
        LinkValidationIssueKind::ContainerPathMissing => 9,
        LinkValidationIssueKind::ContainerPathMismatch => 10,
        LinkValidationIssueKind::ContainerPathUidMismatch => 11,
        LinkValidationIssueKind::DuplicateIncomingRecord => 12,
        LinkValidationIssueKind::DuplicateOutgoingRecord => 13,
        LinkValidationIssueKind::MetadataInvalid => 14,
        LinkValidationIssueKind::MaterializedSymlinkMissing => 15,
        LinkValidationIssueKind::MaterializedSymlinkMismatch => 16,
    }
}

/// Emits a concise debug or warning summary for one validated entry key.
///
/// # Arguments
///
/// * `container_uid` - UID of the current container attached to structured log fields.
/// * `key` - Current entry key attached to structured log fields.
/// * `report` - Completed report whose counts determine level and message.
///
/// # Returns
///
/// Returns after emitting `debug` for a resolved report or `warn` when broken or unavailable items
/// remain.
pub(crate) fn log_validation_report(
    container_uid: &str,
    key: &EntryKey,
    report: &LinkValidationReport,
) {
    if report.broken.is_empty() && report.unavailable.is_empty() {
        tracing::debug!(
            container_uid,
            entry_key = %key,
            valid = report.valid.len(),
            ignored_current_records = report.ignored_current_records,
            ignored_corresponding_records = report.ignored_corresponding_records,
            ignored_containers = report.ignored_containers,
            "link validation completed"
        );
    } else {
        tracing::warn!(
            container_uid,
            entry_key = %key,
            broken = report.broken.len(),
            unavailable = report.unavailable.len(),
            "link validation found unresolved relationships"
        );
    }
}

#[allow(clippy::too_many_arguments)]
/// Appends one fully contextualized broken relationship to a report.
///
/// # Arguments
///
/// * `report` - Destination report whose `broken` vector receives the issue.
/// * `kind` - Precise inconsistency category.
/// * `current` - Current container snapshot supplying its UID.
/// * `current_key` - Entry key being validated in the current container.
/// * `corresponding_uid` - UID of the peer implicated by the issue.
/// * `corresponding_path` - Filesystem root of that peer.
/// * `linker_key` - Optional linker-side key when known.
/// * `target_key` - Optional target-side key when known.
/// * `expected` - Optional expected value rendered for diagnostics.
/// * `actual` - Optional observed value rendered for diagnostics.
///
/// # Returns
///
/// Returns after cloning the supplied context into one new broken issue.
pub(crate) fn push_issue(
    report: &mut LinkValidationReport,
    kind: LinkValidationIssueKind,
    current: &ContainerLinkSnapshot,
    current_key: &EntryKey,
    corresponding_uid: &str,
    corresponding_path: &Path,
    linker_key: Option<&str>,
    target_key: Option<&str>,
    expected: Option<&str>,
    actual: Option<&str>,
) {
    report.broken.push(LinkValidationIssue {
        kind,
        current_container_uid: current.container_uid.clone(),
        current_key: current_key.clone(),
        corresponding_container_uid: corresponding_uid.to_owned(),
        corresponding_container_path: corresponding_path.to_owned(),
        linker_key: linker_key.map(ToOwned::to_owned),
        target_key: target_key.map(ToOwned::to_owned),
        expected: expected.map(ToOwned::to_owned),
        actual: actual.map(ToOwned::to_owned),
    });
}

/// Appends a valid reciprocal relationship only once per full identity tuple.
///
/// # Arguments
///
/// * `report` - Destination report whose `valid` vector may receive a match.
/// * `seen` - Set of full `(linker UID, linker key, target UID, target key)` identities already
///   emitted during this run.
/// * `linker_uid` - Persistent UID of the outgoing-link owner.
/// * `linker_key` - Entry key used by the outgoing link.
/// * `target_uid` - Persistent UID of the target container.
/// * `target_key` - Ordinary entry key in the target container.
///
/// # Returns
///
/// Returns after inserting a new identity and match, or without mutation when already present.
pub(crate) fn push_match(
    report: &mut LinkValidationReport,
    seen: &mut BTreeSet<(String, String, String, String)>,
    linker_uid: &str,
    linker_key: &str,
    target_uid: &str,
    target_key: &str,
) {
    let identity = (
        linker_uid.to_owned(),
        linker_key.to_owned(),
        target_uid.to_owned(),
        target_key.to_owned(),
    );
    if seen.insert(identity.clone()) {
        report.valid.push(LinkMatch {
            linker_container_uid: identity.0,
            linker_key: identity.1,
            target_container_uid: identity.2,
            target_key: identity.3,
        });
    }
}

/// Reports duplicate incoming records within a relevant candidate set.
///
/// # Arguments
///
/// * `owner` - Current snapshot recorded as the validation subject.
/// * `current_key` - Current entry key recorded in duplicate issues.
/// * `corresponding_uid` - UID of the peer associated with the candidate records.
/// * `corresponding_path` - Filesystem root of that peer.
/// * `records` - Incoming records whose target/linker identity tuples are checked for repetition.
/// * `report` - Destination report receiving one issue for every occurrence after the first.
///
/// # Returns
///
/// Returns after examining all records and appending duplicate issues.
pub(crate) fn detect_duplicate_incoming(
    owner: &ContainerLinkSnapshot,
    current_key: &EntryKey,
    corresponding_uid: &str,
    corresponding_path: &Path,
    records: &[&IncomingLinkRecord],
    report: &mut LinkValidationReport,
) {
    let mut seen = BTreeSet::new();
    for record in records {
        let identity = (
            record.target_key.clone(),
            record.linker_uid.clone(),
            record.linker_key.clone(),
        );
        if !seen.insert(identity) {
            push_issue(
                report,
                LinkValidationIssueKind::DuplicateIncomingRecord,
                owner,
                current_key,
                corresponding_uid,
                corresponding_path,
                Some(&record.linker_key),
                Some(&record.target_key),
                None,
                None,
            );
        }
    }
}
