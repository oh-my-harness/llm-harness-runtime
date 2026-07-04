use std::collections::HashMap;

use llm_harness_types::{EntryId, SessionError};

use super::types::{SessionEntry, SessionEntryKind, SessionEntryPayload};

// ── Tree traversal primitives ─────────────────────────────────────────────────
//
// Pure functions over `(entries, children_map)` — the shared in-memory index
// shape used by both `InMemorySessionStorage` and `JsonlSessionStorage`.
// Both backends delegate to these to guarantee identical tree semantics.

/// Get all child entries of the given parent.
pub fn children(
    entries: &HashMap<EntryId, SessionEntry>,
    children_map: &HashMap<Option<EntryId>, Vec<EntryId>>,
    parent: EntryId,
) -> Vec<SessionEntry> {
    let ids = children_map
        .get(&Some(parent))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    ids.iter()
        .filter_map(|id| entries.get(id).cloned())
        .collect()
}

/// Get all leaf entry IDs (entries with no children).
pub fn all_leaves(
    entries: &HashMap<EntryId, SessionEntry>,
    children_map: &HashMap<Option<EntryId>, Vec<EntryId>>,
) -> Vec<EntryId> {
    entries
        .keys()
        .filter(|id| {
            children_map
                .get(&Some(**id))
                .map(|v| v.is_empty())
                .unwrap_or(true)
        })
        .copied()
        .collect()
}

/// Return all entries from `target` to the root, ordered root-first.
pub fn path_to_root(
    entries: &HashMap<EntryId, SessionEntry>,
    target: EntryId,
) -> Result<Vec<SessionEntry>, SessionError> {
    let mut path = Vec::new();
    let mut current = Some(target);
    while let Some(id) = current {
        match entries.get(&id) {
            Some(entry) => {
                current = entry.parent_id;
                path.push(entry.clone());
            }
            None => return Err(SessionError::EntryNotFound(id)),
        }
    }
    path.reverse();
    Ok(path)
}

/// Find the lowest common ancestor of two entries.
pub fn common_ancestor(
    entries: &HashMap<EntryId, SessionEntry>,
    a: EntryId,
    b: EntryId,
) -> Result<Option<EntryId>, SessionError> {
    fn ancestors(
        entries: &HashMap<EntryId, SessionEntry>,
        start: EntryId,
    ) -> Result<Vec<EntryId>, SessionError> {
        let mut v = Vec::new();
        let mut cur = Some(start);
        while let Some(id) = cur {
            v.push(id);
            cur = entries
                .get(&id)
                .ok_or(SessionError::EntryNotFound(id))?
                .parent_id;
        }
        Ok(v)
    }
    let a_anc = ancestors(entries, a)?;
    let b_set: std::collections::HashSet<EntryId> =
        ancestors(entries, b)?.into_iter().collect();
    Ok(a_anc.into_iter().find(|id| b_set.contains(id)))
}

/// Return the label name for the entry at `id`, if any.
pub fn label_at(
    entries: &HashMap<EntryId, SessionEntry>,
    children_map: &HashMap<Option<EntryId>, Vec<EntryId>>,
    id: EntryId,
) -> Option<String> {
    let kids = children_map
        .get(&Some(id))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    for child_id in kids {
        if let Some(e) = entries.get(child_id)
            && let SessionEntryPayload::Label { name } = &e.payload
        {
            return Some(name.clone());
        }
    }
    None
}

/// Return all leaf-to-root paths. Each inner `Vec` is ordered root-first.
///
/// If any leaf's parent chain hits a missing entry (data corruption / orphan),
/// returns `Err(EntryNotFound)` rather than silently returning a partial path.
pub fn paths_to_all_leaves(
    entries: &HashMap<EntryId, SessionEntry>,
    children_map: &HashMap<Option<EntryId>, Vec<EntryId>>,
) -> Result<Vec<Vec<SessionEntry>>, SessionError> {
    let leaves: Vec<EntryId> = entries
        .keys()
        .filter(|id| {
            children_map
                .get(&Some(**id))
                .map(|v| v.is_empty())
                .unwrap_or(true)
        })
        .copied()
        .collect();
    let mut result = Vec::with_capacity(leaves.len());
    for leaf_id in leaves {
        let mut path = Vec::new();
        let mut current = Some(leaf_id);
        while let Some(id) = current {
            match entries.get(&id) {
                Some(e) => {
                    path.push(e.clone());
                    current = e.parent_id;
                }
                None => return Err(SessionError::EntryNotFound(id)),
            }
        }
        path.reverse();
        result.push(path);
    }
    Ok(result)
}

/// Find all entries of a given kind.
pub fn find_entries_by_type(
    entries: &HashMap<EntryId, SessionEntry>,
    kind: SessionEntryKind,
) -> Vec<EntryId> {
    entries
        .values()
        .filter(|e| e.kind() == kind)
        .map(|e| e.id)
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(parent: Option<EntryId>, payload: SessionEntryPayload) -> SessionEntry {
        SessionEntry {
            id: EntryId::new(),
            parent_id: parent,
            timestamp: chrono::Utc::now(),
            payload,
        }
    }

    fn label(name: &str) -> SessionEntryPayload {
        SessionEntryPayload::Label {
            name: name.into(),
        }
    }

    /// Insert `entry` into `entries` and register it under `entry.parent_id` in `children`.
    fn index(
        entries: &mut HashMap<EntryId, SessionEntry>,
        children: &mut HashMap<Option<EntryId>, Vec<EntryId>>,
        entry: SessionEntry,
    ) -> EntryId {
        let id = entry.id;
        children.entry(entry.parent_id).or_default().push(id);
        entries.insert(id, entry);
        id
    }

    /// Build a two-branch tree:
    ///   root
    ///   ├── A
    ///   │   └── A_leaf
    ///   └── B
    ///       └── B_leaf
    fn two_branch_tree(
    ) -> (HashMap<EntryId, SessionEntry>, HashMap<Option<EntryId>, Vec<EntryId>>) {
        let mut entries = HashMap::new();
        let mut children = HashMap::new();

        let root = make_entry(None, label("root"));
        let root_id = index(&mut entries, &mut children, root);

        let a = make_entry(Some(root_id), label("A"));
        let a_id = index(&mut entries, &mut children, a);
        let a_leaf = make_entry(Some(a_id), label("A_leaf"));
        let _ = index(&mut entries, &mut children, a_leaf);

        let b = make_entry(Some(root_id), label("B"));
        let b_id = index(&mut entries, &mut children, b);
        let b_leaf = make_entry(Some(b_id), label("B_leaf"));
        let _ = index(&mut entries, &mut children, b_leaf);

        (entries, children)
    }

    #[test]
    fn children_returns_direct_children() {
        let (entries, children) = two_branch_tree();
        let root_id = *children.get(&None).unwrap().first().unwrap();
        let result = super::children(&entries, &children, root_id);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn children_empty_for_leaf() {
        let (entries, children) = two_branch_tree();
        let leaf_id = entries
            .keys()
            .find(|id| !children.get(&Some(**id)).is_some_and(|v| !v.is_empty()))
            .copied()
            .unwrap();
        let result = super::children(&entries, &children, leaf_id);
        assert!(result.is_empty());
    }

    #[test]
    fn all_leaves_finds_two_leaves() {
        let (entries, children) = two_branch_tree();
        let leaves = super::all_leaves(&entries, &children);
        assert_eq!(leaves.len(), 2);
    }

    #[test]
    fn path_to_root_returns_root_first() {
        let (entries, children) = two_branch_tree();
        let leaf_id = super::all_leaves(&entries, &children)[0];
        let path = super::path_to_root(&entries, leaf_id).unwrap();
        assert!(path.len() == 3); // leaf → mid → root
        assert_eq!(path[0].parent_id, None); // first element is root
        assert_eq!(path.last().unwrap().id, leaf_id); // last element is leaf
    }

    #[test]
    fn path_to_root_missing_target_returns_err() {
        let (entries, _children) = two_branch_tree();
        let missing = EntryId::new();
        let result = super::path_to_root(&entries, missing);
        assert!(matches!(result, Err(SessionError::EntryNotFound(_))));
    }

    #[test]
    fn common_ancestor_of_two_leaves_is_root() {
        let (entries, children) = two_branch_tree();
        let leaves = super::all_leaves(&entries, &children);
        let ancestor = super::common_ancestor(&entries, leaves[0], leaves[1]).unwrap();
        assert!(ancestor.is_some());
        let root_id = *children.get(&None).unwrap().first().unwrap();
        assert_eq!(ancestor.unwrap(), root_id);
    }

    #[test]
    fn label_at_finds_first_label_child() {
        let (entries, children) = two_branch_tree();
        let root_id = *children.get(&None).unwrap().first().unwrap();
        // Root's first child is "A" (a Label entry).
        let result = super::label_at(&entries, &children, root_id);
        assert_eq!(result.as_deref(), Some("A"));
    }

    #[test]
    fn label_at_none_when_no_label_child() {
        let (entries, children) = two_branch_tree();
        let leaves = super::all_leaves(&entries, &children);
        // Leaves have no children → no label.
        let result = super::label_at(&entries, &children, leaves[0]);
        assert_eq!(result, None);
    }

    #[test]
    fn paths_to_all_leaves_returns_two_paths() {
        let (entries, children) = two_branch_tree();
        let paths = super::paths_to_all_leaves(&entries, &children).unwrap();
        assert_eq!(paths.len(), 2);
        for path in &paths {
            assert_eq!(path[0].parent_id, None); // each starts at root
        }
    }

    #[test]
    fn paths_to_all_leaves_orphan_returns_err() {
        // Build a tree with an orphan: entry whose parent_id points to a
        // non-existent entry. This is the data-corruption case unified on Err.
        let mut entries = HashMap::new();
        let mut children_map = HashMap::new();

        let root = make_entry(None, label("root"));
        let _ = index(&mut entries, &mut children_map, root);

        // Orphan: parent_id is a phantom (non-existent) ID.
        let phantom = EntryId::new();
        let orphan = make_entry(Some(phantom), label("orphan"));
        let _ = index(&mut entries, &mut children_map, orphan);

        let result = super::paths_to_all_leaves(&entries, &children_map);
        assert!(
            matches!(result, Err(SessionError::EntryNotFound(_))),
            "orphan parent chain must surface as Err, not silent partial path"
        );
    }

    #[test]
    fn find_entries_by_type_label() {
        let (entries, _children) = two_branch_tree();
        let ids = super::find_entries_by_type(&entries, SessionEntryKind::Label);
        assert_eq!(ids.len(), 5); // root, A, A_leaf, B, B_leaf — all Label
    }

    #[test]
    fn find_entries_by_type_none_for_absent_kind() {
        let (entries, _children) = two_branch_tree();
        let ids = super::find_entries_by_type(&entries, SessionEntryKind::Message);
        assert!(ids.is_empty());
    }
}
