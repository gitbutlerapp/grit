//! Revision-selection daily matrix registration (factory step 2399).

use grit_utils::daily::areas::revision_selection;
use grit_utils::daily::REVISION_SELECTION_MATRIX_SCENARIOS;

#[test]
fn revision_selection_matrix_ids_have_daily_plans() {
    let plan_ids: std::collections::HashSet<_> = revision_selection::plans()
        .iter()
        .map(|p| p.spec.id)
        .collect();
    for spec in REVISION_SELECTION_MATRIX_SCENARIOS {
        assert!(
            plan_ids.contains(spec.id),
            "missing daily plan for revision-selection matrix id {}",
            spec.id
        );
    }
}

#[test]
fn revision_selection_matrix_ids_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for spec in REVISION_SELECTION_MATRIX_SCENARIOS {
        assert!(
            seen.insert(spec.id),
            "duplicate revision-selection matrix id {}",
            spec.id
        );
    }
}
