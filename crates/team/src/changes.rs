//! Which parts of a team project an edit changed: sequences (sent under their lock) and shared
//! items or other shared data (sent to the shared part).

use std::collections::BTreeSet;
use std::sync::Arc;

use filmcraft_project::{ItemId, ItemKind, Project, ProjectItem};
use serde::Serialize;

/// The parts of a project that differ between two snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Sequence items that were edited, added or removed.
    pub sequences: BTreeSet<ItemId>,
    /// Other items (media, subclips, adjustment layers…) that were edited, added or removed.
    pub shared_items: BTreeSet<ItemId>,
    /// Anything else in the shared part changed: bins, transcripts, source graphics, narrations,
    /// settings, notes, the project name, colours, LUTs or search bins.
    pub shared_other: bool,
}

impl ChangeSet {
    /// Nothing changed.
    pub fn is_empty(&self) -> bool {
        self.sequences.is_empty() && self.shared_items.is_empty() && !self.shared_other
    }
}

/// Compare two snapshots of a project, normally the one before a command and the one after.
/// Sequences are compared by pointer first (undo snapshots share untouched sequences), then by
/// value (NaN-safe, see `same`). `next_id` and `id_slot` are not compared: they are this Mac's
/// allocator state.
pub fn classify(before: &Project, after: &Project) -> ChangeSet {
    let mut out = ChangeSet::default();
    let ids: BTreeSet<ItemId> = before.items.keys().chain(after.items.keys()).copied().collect();
    for id in ids {
        let (b, a) = (before.items.get(&id), after.items.get(&id));
        if items_equal(b, a) {
            continue;
        }
        if b.is_some_and(is_sequence) || a.is_some_and(is_sequence) {
            out.sequences.insert(id);
        }
        if b.is_some_and(|i| !is_sequence(i)) || a.is_some_and(|i| !is_sequence(i)) {
            out.shared_items.insert(id);
        }
    }
    out.shared_other = shared_other_changed(before, after);
    out
}

fn is_sequence(item: &ProjectItem) -> bool {
    matches!(item.kind, ItemKind::Sequence(_))
}

fn items_equal(b: Option<&ProjectItem>, a: Option<&ProjectItem>) -> bool {
    match (b, a) {
        (None, None) => true,
        (Some(b), Some(a)) => item_equal(b, a),
        _ => false,
    }
}

/// Equal items. Two sequence items holding the same `Arc` skip the deep comparison of the
/// timeline; every other field is still compared.
fn item_equal(b: &ProjectItem, a: &ProjectItem) -> bool {
    if let (ItemKind::Sequence(sb), ItemKind::Sequence(sa)) = (&b.kind, &a.kind)
        && Arc::ptr_eq(sb, sa)
    {
        // Listed field by field so a new field in upstream `ProjectItem` fails to compile here.
        let ProjectItem { id, name, label, custom_label_color, kind: _, metadata, created, split } = b;
        return *id == a.id
            && *name == a.name
            && *label == a.label
            && *custom_label_color == a.custom_label_color
            && *metadata == a.metadata
            && *created == a.created
            && same(split, &a.split);
    }
    same(b, a)
}

/// Equal values. `==` first; when it says no, compare as JSON, because unset point parameters
/// (the frame centre) are NaN and NaN is never `==` to itself. NaN is written as `null` in JSON,
/// the same as in a saved project. A value that cannot be serialized counts as changed.
fn same<T: PartialEq + Serialize>(b: &T, a: &T) -> bool {
    b == a
        || match (serde_json::to_value(b), serde_json::to_value(a)) {
            (Ok(b), Ok(a)) => b == a,
            _ => false,
        }
}

/// Whether anything in the shared part other than the items differs.
fn shared_other_changed(before: &Project, after: &Project) -> bool {
    // Listed field by field so a new field in upstream `Project` fails to compile here.
    let Project {
        name,
        custom_colors,
        custom_color_names,
        settings,
        root,
        items: _,
        next_id: _,
        luts,
        transcripts,
        search_bins,
        source_graphics,
        narrations,
        notes,
        id_slot: _,
    } = before;
    !(*name == after.name
        && same(custom_colors, &after.custom_colors)
        && same(custom_color_names, &after.custom_color_names)
        && same(settings, &after.settings)
        && same(root, &after.root)
        && same(luts, &after.luts)
        && same(transcripts, &after.transcripts)
        && same(search_bins, &after.search_bins)
        && same(source_graphics, &after.source_graphics)
        && same(narrations, &after.narrations)
        && *notes == after.notes)
}
