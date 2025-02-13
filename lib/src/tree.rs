// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;
use std::fmt::{Debug, Error, Formatter};
use std::io::Read;
use std::iter::Peekable;
use std::pin::Pin;
use std::sync::Arc;

use itertools::Itertools;
use thiserror::Error;

use crate::backend::{
    BackendError, Conflict, ConflictId, ConflictTerm, FileId, ObjectId,
    TreeEntriesNonRecursiveIterator, TreeEntry, TreeId, TreeValue,
};
use crate::files::MergeResult;
use crate::matchers::{EverythingMatcher, Matcher};
use crate::repo_path::{RepoPath, RepoPathComponent, RepoPathJoin};
use crate::store::Store;
use crate::{backend, files};

#[derive(Debug, Error)]
pub enum TreeMergeError {
    #[error("Failed to read file with ID {} ", .file_id.hex())]
    ReadError {
        source: std::io::Error,
        file_id: FileId,
    },
    #[error("Backend error: {0}")]
    BackendError(#[from] BackendError),
}

#[derive(Clone)]
pub struct Tree {
    store: Arc<Store>,
    dir: RepoPath,
    id: TreeId,
    data: Arc<backend::Tree>,
}

impl Debug for Tree {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        f.debug_struct("Tree")
            .field("dir", &self.dir)
            .field("id", &self.id)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct DiffSummary {
    pub modified: Vec<RepoPath>,
    pub added: Vec<RepoPath>,
    pub removed: Vec<RepoPath>,
}

impl DiffSummary {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

impl Tree {
    pub fn new(store: Arc<Store>, dir: RepoPath, id: TreeId, data: Arc<backend::Tree>) -> Self {
        Tree {
            store,
            dir,
            id,
            data,
        }
    }

    pub fn null(store: Arc<Store>, dir: RepoPath) -> Self {
        Tree {
            store,
            dir,
            id: TreeId::new(vec![]),
            data: Arc::new(backend::Tree::default()),
        }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn dir(&self) -> &RepoPath {
        &self.dir
    }

    pub fn id(&self) -> &TreeId {
        &self.id
    }

    pub fn data(&self) -> &backend::Tree {
        &self.data
    }

    pub fn entries_non_recursive(&self) -> TreeEntriesNonRecursiveIterator {
        self.data.entries()
    }

    pub fn entries(&self) -> TreeEntriesIterator<'static> {
        TreeEntriesIterator::new(self.clone(), &EverythingMatcher)
    }

    pub fn entries_matching<'matcher>(
        &self,
        matcher: &'matcher dyn Matcher,
    ) -> TreeEntriesIterator<'matcher> {
        TreeEntriesIterator::new(self.clone(), matcher)
    }

    pub fn entry(&self, basename: &RepoPathComponent) -> Option<TreeEntry> {
        self.data.entry(basename)
    }

    pub fn value(&self, basename: &RepoPathComponent) -> Option<&TreeValue> {
        self.data.value(basename)
    }

    pub fn path_value(&self, path: &RepoPath) -> Option<TreeValue> {
        assert_eq!(self.dir(), &RepoPath::root());
        match path.split() {
            Some((dir, basename)) => self
                .sub_tree_recursive(dir.components())
                .and_then(|tree| tree.data.value(basename).cloned()),
            None => Some(TreeValue::Tree(self.id.clone())),
        }
    }

    pub fn sub_tree(&self, name: &RepoPathComponent) -> Option<Tree> {
        self.data.value(name).and_then(|sub_tree| match sub_tree {
            TreeValue::Tree(sub_tree_id) => {
                let subdir = self.dir.join(name);
                Some(self.store.get_tree(&subdir, sub_tree_id).unwrap())
            }
            _ => None,
        })
    }

    pub fn known_sub_tree(&self, name: &RepoPathComponent, id: &TreeId) -> Tree {
        let subdir = self.dir.join(name);
        self.store.get_tree(&subdir, id).unwrap()
    }

    fn sub_tree_recursive(&self, components: &[RepoPathComponent]) -> Option<Tree> {
        if components.is_empty() {
            // TODO: It would be nice to be able to return a reference here, but
            // then we would have to figure out how to share Tree instances
            // across threads.
            Some(Tree {
                store: self.store.clone(),
                dir: self.dir.clone(),
                id: self.id.clone(),
                data: self.data.clone(),
            })
        } else {
            match self.data.entry(&components[0]) {
                None => None,
                Some(entry) => match entry.value() {
                    TreeValue::Tree(sub_tree_id) => {
                        let sub_tree = self.known_sub_tree(entry.name(), sub_tree_id);
                        sub_tree.sub_tree_recursive(&components[1..])
                    }
                    _ => None,
                },
            }
        }
    }

    pub fn diff<'matcher>(
        &self,
        other: &Tree,
        matcher: &'matcher dyn Matcher,
    ) -> TreeDiffIterator<'matcher> {
        recursive_tree_diff(self.clone(), other.clone(), matcher)
    }

    pub fn diff_summary(&self, other: &Tree, matcher: &dyn Matcher) -> DiffSummary {
        let mut modified = vec![];
        let mut added = vec![];
        let mut removed = vec![];
        for (file, diff) in self.diff(other, matcher) {
            match diff {
                Diff::Modified(_, _) => modified.push(file.clone()),
                Diff::Added(_) => added.push(file.clone()),
                Diff::Removed(_) => removed.push(file.clone()),
            }
        }
        modified.sort();
        added.sort();
        removed.sort();
        DiffSummary {
            modified,
            added,
            removed,
        }
    }

    pub fn conflicts_matching(&self, matcher: &dyn Matcher) -> Vec<(RepoPath, ConflictId)> {
        let mut conflicts = vec![];
        for (name, value) in self.entries_matching(matcher) {
            if let TreeValue::Conflict(id) = value {
                conflicts.push((name.clone(), id.clone()));
            }
        }
        conflicts
    }

    pub fn conflicts(&self) -> Vec<(RepoPath, ConflictId)> {
        self.conflicts_matching(&EverythingMatcher)
    }

    pub fn has_conflict(&self) -> bool {
        !self.conflicts().is_empty()
    }
}

pub struct TreeEntriesIterator<'matcher> {
    entry_iterator: TreeEntriesNonRecursiveIterator<'static>,
    // On drop, tree must outlive entry_iterator
    tree: Pin<Box<Tree>>,
    subdir_iterator: Option<Box<TreeEntriesIterator<'matcher>>>,
    matcher: &'matcher dyn Matcher,
}

impl<'matcher> TreeEntriesIterator<'matcher> {
    fn new(tree: Tree, matcher: &'matcher dyn Matcher) -> Self {
        let tree = Box::pin(tree);
        // TODO: Restrict walk according to Matcher::visit()
        let entry_iterator = tree.entries_non_recursive();
        let entry_iterator: TreeEntriesNonRecursiveIterator<'static> =
            unsafe { std::mem::transmute(entry_iterator) };
        Self {
            entry_iterator,
            tree,
            subdir_iterator: None,
            matcher,
        }
    }
}

impl Iterator for TreeEntriesIterator<'_> {
    type Item = (RepoPath, TreeValue);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // First return results from any subdirectory we're currently visiting.
            if let Some(subdir_iter) = &mut self.subdir_iterator {
                if let Some(item) = subdir_iter.next() {
                    return Some(item);
                }
                self.subdir_iterator = None;
            }
            let entry = self.entry_iterator.next()?;
            match entry.value() {
                TreeValue::Tree(id) => {
                    let subtree = self.tree.known_sub_tree(entry.name(), id);
                    self.subdir_iterator =
                        Some(Box::new(TreeEntriesIterator::new(subtree, self.matcher)));
                }
                other => {
                    let path = self.tree.dir().join(entry.name());
                    if !self.matcher.matches(&path) {
                        continue;
                    }
                    return Some((path, other.clone()));
                }
            };
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Diff<T> {
    Modified(T, T),
    Added(T),
    Removed(T),
}

impl<T> Diff<T> {
    pub fn as_options(&self) -> (Option<&T>, Option<&T>) {
        match self {
            Diff::Modified(left, right) => (Some(left), Some(right)),
            Diff::Added(right) => (None, Some(right)),
            Diff::Removed(left) => (Some(left), None),
        }
    }

    pub fn into_options(self) -> (Option<T>, Option<T>) {
        match self {
            Diff::Modified(left, right) => (Some(left), Some(right)),
            Diff::Added(right) => (None, Some(right)),
            Diff::Removed(left) => (Some(left), None),
        }
    }
}

struct TreeEntryDiffIterator<'trees> {
    it1: Peekable<TreeEntriesNonRecursiveIterator<'trees>>,
    it2: Peekable<TreeEntriesNonRecursiveIterator<'trees>>,
}

impl<'trees> TreeEntryDiffIterator<'trees> {
    fn new(tree1: &'trees Tree, tree2: &'trees Tree) -> Self {
        let it1 = tree1.entries_non_recursive().peekable();
        let it2 = tree2.entries_non_recursive().peekable();
        TreeEntryDiffIterator { it1, it2 }
    }
}

impl<'trees> Iterator for TreeEntryDiffIterator<'trees> {
    type Item = (
        &'trees RepoPathComponent,
        Option<&'trees TreeValue>,
        Option<&'trees TreeValue>,
    );

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry1 = self.it1.peek();
            let entry2 = self.it2.peek();
            match (&entry1, &entry2) {
                (Some(before), Some(after)) => {
                    match before.name().cmp(after.name()) {
                        Ordering::Less => {
                            // entry removed
                            let before = self.it1.next().unwrap();
                            return Some((before.name(), Some(before.value()), None));
                        }
                        Ordering::Greater => {
                            // entry added
                            let after = self.it2.next().unwrap();
                            return Some((after.name(), None, Some(after.value())));
                        }
                        Ordering::Equal => {
                            // entry modified or clean
                            let before = self.it1.next().unwrap();
                            let after = self.it2.next().unwrap();
                            if before.value() != after.value() {
                                return Some((
                                    before.name(),
                                    Some(before.value()),
                                    Some(after.value()),
                                ));
                            }
                        }
                    }
                }
                (Some(_), None) => {
                    // second iterator exhausted
                    let before = self.it1.next().unwrap();
                    return Some((before.name(), Some(before.value()), None));
                }
                (None, Some(_)) => {
                    // first iterator exhausted
                    let after = self.it2.next().unwrap();
                    return Some((after.name(), None, Some(after.value())));
                }
                (None, None) => {
                    // both iterators exhausted
                    return None;
                }
            }
        }
    }
}

fn diff_entries<'trees>(tree1: &'trees Tree, tree2: &'trees Tree) -> TreeEntryDiffIterator<'trees> {
    TreeEntryDiffIterator::new(tree1, tree2)
}

pub fn recursive_tree_diff(root1: Tree, root2: Tree, matcher: &dyn Matcher) -> TreeDiffIterator {
    TreeDiffIterator::new(RepoPath::root(), root1, root2, matcher)
}

pub struct TreeDiffIterator<'matcher> {
    stack: Vec<TreeDiffItem>,
    matcher: &'matcher dyn Matcher,
}

struct TreeDiffDirItem {
    path: RepoPath,
    // Iterator over the diffs between tree1 and tree2
    entry_iterator: TreeEntryDiffIterator<'static>,
    // On drop, tree1 and tree2 must outlive entry_iterator
    tree1: Pin<Box<Tree>>,
    tree2: Pin<Box<Tree>>,
}

enum TreeDiffItem {
    Dir(TreeDiffDirItem),
    // This is used for making sure that when a directory gets replaced by a file, we
    // yield the value for the addition of the file after we yield the values
    // for removing files in the directory.
    File(RepoPath, Diff<TreeValue>),
}

impl<'matcher> TreeDiffIterator<'matcher> {
    fn new(dir: RepoPath, tree1: Tree, tree2: Tree, matcher: &'matcher dyn Matcher) -> Self {
        let mut stack = Vec::new();
        if !matcher.visit(&dir).is_nothing() {
            stack.push(TreeDiffItem::Dir(TreeDiffDirItem::new(dir, tree1, tree2)));
        };
        Self { stack, matcher }
    }
}

impl TreeDiffDirItem {
    fn new(path: RepoPath, tree1: Tree, tree2: Tree) -> Self {
        let tree1 = Box::pin(tree1);
        let tree2 = Box::pin(tree2);
        let iter: TreeEntryDiffIterator = diff_entries(&tree1, &tree2);
        let iter: TreeEntryDiffIterator<'static> = unsafe { std::mem::transmute(iter) };
        Self {
            path,
            entry_iterator: iter,
            tree1,
            tree2,
        }
    }

    fn subdir(
        &self,
        name: &RepoPathComponent,
        before: Option<&TreeValue>,
        after: Option<&TreeValue>,
    ) -> Self {
        let subdir_path = self.path.join(name);
        let before_tree = match before {
            Some(TreeValue::Tree(id_before)) => self.tree1.known_sub_tree(name, id_before),
            _ => Tree::null(self.tree1.store().clone(), subdir_path.clone()),
        };
        let after_tree = match after {
            Some(TreeValue::Tree(id_after)) => self.tree2.known_sub_tree(name, id_after),
            _ => Tree::null(self.tree2.store().clone(), subdir_path.clone()),
        };
        Self::new(subdir_path, before_tree, after_tree)
    }
}

impl Iterator for TreeDiffIterator<'_> {
    type Item = (RepoPath, Diff<TreeValue>);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(top) = self.stack.last_mut() {
            let (dir, (name, before, after)) = match top {
                TreeDiffItem::Dir(dir) => {
                    if let Some(entry) = dir.entry_iterator.next() {
                        (dir, entry)
                    } else {
                        self.stack.pop().unwrap();
                        continue;
                    }
                }
                TreeDiffItem::File(..) => {
                    if let TreeDiffItem::File(name, diff) = self.stack.pop().unwrap() {
                        return Some((name, diff));
                    } else {
                        unreachable!();
                    }
                }
            };

            // Note: whenever we say "file" below, it may also be a symlink or a conflict.
            let file_path = dir.path.join(name);
            let tree_before = matches!(before, Some(TreeValue::Tree(_)));
            let tree_after = matches!(after, Some(TreeValue::Tree(_)));
            let post_subdir =
                if (tree_before || tree_after) && !self.matcher.visit(&file_path).is_nothing() {
                    let subdir = dir.subdir(name, before, after);
                    self.stack.push(TreeDiffItem::Dir(subdir));
                    self.stack.len() - 1
                } else {
                    self.stack.len()
                };
            if self.matcher.matches(&file_path) {
                if !tree_before && tree_after {
                    if let Some(file_before) = before {
                        return Some((file_path, Diff::Removed(file_before.clone())));
                    }
                } else if tree_before && !tree_after {
                    if let Some(file_after) = after {
                        self.stack.insert(
                            post_subdir,
                            TreeDiffItem::File(file_path, Diff::Added(file_after.clone())),
                        );
                    }
                } else if !tree_before && !tree_after {
                    match (before, after) {
                        (Some(file_before), Some(file_after)) => {
                            return Some((
                                file_path,
                                Diff::Modified(file_before.clone(), file_after.clone()),
                            ));
                        }
                        (None, Some(file_after)) => {
                            return Some((file_path, Diff::Added(file_after.clone())));
                        }
                        (Some(file_before), None) => {
                            return Some((file_path, Diff::Removed(file_before.clone())));
                        }
                        (None, None) => {
                            panic!("unexpected diff")
                        }
                    }
                }
            }
        }
        None
    }
}

pub fn merge_trees(
    side1_tree: &Tree,
    base_tree: &Tree,
    side2_tree: &Tree,
) -> Result<TreeId, TreeMergeError> {
    let store = base_tree.store();
    let dir = base_tree.dir();
    assert_eq!(side1_tree.dir(), dir);
    assert_eq!(side2_tree.dir(), dir);

    if base_tree.id() == side1_tree.id() {
        return Ok(side2_tree.id().clone());
    }
    if base_tree.id() == side2_tree.id() || side1_tree.id() == side2_tree.id() {
        return Ok(side1_tree.id().clone());
    }

    // Start with a tree identical to side 1 and modify based on changes from base
    // to side 2.
    let mut new_tree = side1_tree.data().clone();
    for (basename, maybe_base, maybe_side2) in diff_entries(base_tree, side2_tree) {
        let maybe_side1 = side1_tree.value(basename);
        if maybe_side1 == maybe_base {
            // side 1 is unchanged: use the value from side 2
            match maybe_side2 {
                None => new_tree.remove(basename),
                Some(side2) => new_tree.set(basename.clone(), side2.clone()),
            };
        } else if maybe_side1 == maybe_side2 {
            // Both sides changed in the same way: new_tree already has the
            // value
        } else {
            // The two sides changed in different ways
            let new_value =
                merge_tree_value(store, dir, basename, maybe_base, maybe_side1, maybe_side2)?;
            match new_value {
                None => new_tree.remove(basename),
                Some(value) => new_tree.set(basename.clone(), value),
            }
        }
    }
    Ok(store.write_tree(dir, &new_tree)?)
}

/// Returns `Some(TreeId)` if this is a directory or missing. If it's missing,
/// we treat it as an empty tree.
fn maybe_tree_id<'id>(
    value: Option<&'id TreeValue>,
    empty_tree_id: &'id TreeId,
) -> Option<&'id TreeId> {
    match value {
        Some(TreeValue::Tree(id)) => Some(id),
        None => Some(empty_tree_id),
        _ => None,
    }
}

fn merge_tree_value(
    store: &Arc<Store>,
    dir: &RepoPath,
    basename: &RepoPathComponent,
    maybe_base: Option<&TreeValue>,
    maybe_side1: Option<&TreeValue>,
    maybe_side2: Option<&TreeValue>,
) -> Result<Option<TreeValue>, TreeMergeError> {
    // Resolve non-trivial conflicts:
    //   * resolve tree conflicts by recursing
    //   * try to resolve file conflicts by merging the file contents
    //   * leave other conflicts (e.g. file/dir conflicts, remove/modify conflicts)
    //     unresolved

    let empty_tree_id = store.empty_tree_id();
    let base_tree_id = maybe_tree_id(maybe_base, empty_tree_id);
    let side1_tree_id = maybe_tree_id(maybe_side1, empty_tree_id);
    let side2_tree_id = maybe_tree_id(maybe_side2, empty_tree_id);
    Ok(match (base_tree_id, side1_tree_id, side2_tree_id) {
        (Some(base_id), Some(side1_id), Some(side2_id)) => {
            let subdir = dir.join(basename);
            let base_tree = store.get_tree(&subdir, base_id)?;
            let side1_tree = store.get_tree(&subdir, side1_id)?;
            let side2_tree = store.get_tree(&subdir, side2_id)?;
            let merged_tree_id = merge_trees(&side1_tree, &base_tree, &side2_tree)?;
            if merged_tree_id == *empty_tree_id {
                None
            } else {
                Some(TreeValue::Tree(merged_tree_id))
            }
        }
        _ => {
            // Start by creating a Conflict object. Conflicts can cleanly represent a single
            // resolved state, the absence of a state, or a conflicted state.
            let mut conflict = Conflict::default();
            if let Some(base) = maybe_base {
                conflict.removes.push(ConflictTerm {
                    value: base.clone(),
                });
            }
            if let Some(side1) = maybe_side1 {
                conflict.adds.push(ConflictTerm {
                    value: side1.clone(),
                });
            }
            if let Some(side2) = maybe_side2 {
                conflict.adds.push(ConflictTerm {
                    value: side2.clone(),
                });
            }
            let filename = dir.join(basename);
            let conflict = simplify_conflict(store, &filename, conflict)?;
            if conflict.adds.is_empty() {
                // If there are no values to add, then the path doesn't exist
                return Ok(None);
            }
            if conflict.removes.is_empty() && conflict.adds.len() == 1 {
                // A single add means that the current state is that state.
                return Ok(Some(conflict.adds[0].value.clone()));
            }
            if let Some((merged_content, executable)) =
                try_resolve_file_conflict(store, &filename, &conflict)?
            {
                let id = store.write_file(&filename, &mut merged_content.as_slice())?;
                Some(TreeValue::File { id, executable })
            } else {
                let conflict_id = store.write_conflict(&filename, &conflict)?;
                Some(TreeValue::Conflict(conflict_id))
            }
        }
    })
}

fn try_resolve_file_conflict(
    store: &Store,
    filename: &RepoPath,
    conflict: &Conflict,
) -> Result<Option<(Vec<u8>, bool)>, TreeMergeError> {
    // If the file was missing from any side (typically a modify/delete conflict),
    // we can't automatically merge it.
    if conflict.adds.len() != conflict.removes.len() + 1 {
        return Ok(None);
    }

    // If there are any non-file parts in the conflict, we can't merge it. We check
    // early so we don't waste time reading file contents if we can't merge them
    // anyway. At the same time we determine whether the resulting file should
    // be executable.
    let mut exec_delta = 0;
    let mut regular_delta = 0;
    let mut removed_file_ids = vec![];
    let mut added_file_ids = vec![];
    for term in &conflict.removes {
        match &term.value {
            TreeValue::File { id, executable } => {
                if *executable {
                    exec_delta -= 1;
                } else {
                    regular_delta -= 1;
                }
                removed_file_ids.push(id.clone());
            }
            _ => {
                return Ok(None);
            }
        }
    }
    for term in &conflict.adds {
        match &term.value {
            TreeValue::File { id, executable } => {
                if *executable {
                    exec_delta += 1;
                } else {
                    regular_delta += 1;
                }
                added_file_ids.push(id.clone());
            }
            _ => {
                return Ok(None);
            }
        }
    }
    let executable = if exec_delta > 0 && regular_delta <= 0 {
        true
    } else if regular_delta > 0 && exec_delta <= 0 {
        false
    } else {
        // We're unable to determine whether the result should be executable
        return Ok(None);
    };
    let mut removed_contents = vec![];
    let mut added_contents = vec![];
    for file_id in removed_file_ids {
        let mut content = vec![];
        store
            .read_file(filename, &file_id)?
            .read_to_end(&mut content)
            .map_err(|err| TreeMergeError::ReadError {
                source: err,
                file_id,
            })?;
        removed_contents.push(content);
    }
    for file_id in added_file_ids {
        let mut content = vec![];
        store
            .read_file(filename, &file_id)?
            .read_to_end(&mut content)
            .map_err(|err| TreeMergeError::ReadError {
                source: err,
                file_id,
            })?;
        added_contents.push(content);
    }
    let merge_result = files::merge(
        &removed_contents.iter().map(Vec::as_slice).collect_vec(),
        &added_contents.iter().map(Vec::as_slice).collect_vec(),
    );
    match merge_result {
        MergeResult::Resolved(merged_content) => Ok(Some((merged_content, executable))),
        MergeResult::Conflict(_) => Ok(None),
    }
}

fn tree_value_to_conflict(
    store: &Store,
    path: &RepoPath,
    value: TreeValue,
) -> Result<Conflict, BackendError> {
    match value {
        TreeValue::Conflict(id) => {
            let conflict = store.read_conflict(path, &id)?;
            Ok(conflict)
        }
        other => Ok(Conflict {
            removes: vec![],
            adds: vec![ConflictTerm { value: other }],
        }),
    }
}

fn simplify_conflict(
    store: &Store,
    path: &RepoPath,
    conflict: Conflict,
) -> Result<Conflict, BackendError> {
    // Important cases to simplify:
    //
    // D
    // |
    // B C
    // |/
    // A
    //
    // 1. rebase C to B, then back to A => there should be no conflict
    // 2. rebase C to B, then to D => the conflict should not mention B
    // 3. rebase B to C and D to B', then resolve the conflict in B' and rebase D'
    // on top =>    the conflict should be between B'', B, and D; it should not
    // mention the conflict in B'

    // Case 1 above:
    // After first rebase, the conflict is {+B-A+C}. After rebasing back,
    // the unsimplified conflict is {+A-B+{+B-A+C}}. Since the
    // inner conflict is positive, we can simply move it into the outer conflict. We
    // thus get {+A-B+B-A+C}, which we can then simplify to just C (because {+C} ==
    // C).
    //
    // Case 2 above:
    // After first rebase, the conflict is {+B-A+C}. After rebasing to D,
    // the unsimplified conflict is {+D-C+{+B-A+C}}. As in the
    // previous case, the inner conflict can be moved into the outer one. We then
    // get {+D-C+B-A+C}. That can be simplified to
    // {+D+B-A}, which is the desired conflict.
    //
    // Case 3 above:
    // TODO: describe this case

    // First expand any diffs with nested conflicts.
    let mut new_removes = vec![];
    let mut new_adds = vec![];
    for term in conflict.adds {
        match term.value {
            TreeValue::Conflict(_) => {
                let conflict = tree_value_to_conflict(store, path, term.value)?;
                new_removes.extend_from_slice(&conflict.removes);
                new_adds.extend_from_slice(&conflict.adds);
            }
            _ => {
                new_adds.push(term);
            }
        }
    }
    for term in conflict.removes {
        match term.value {
            TreeValue::Conflict(_) => {
                let conflict = tree_value_to_conflict(store, path, term.value)?;
                new_removes.extend_from_slice(&conflict.adds);
                new_adds.extend_from_slice(&conflict.removes);
            }
            _ => {
                new_removes.push(term);
            }
        }
    }

    // Remove pairs of entries that match in the removes and adds.
    let mut add_index = 0;
    while add_index < new_adds.len() {
        let add = &new_adds[add_index];
        add_index += 1;
        for (remove_index, remove) in new_removes.iter().enumerate() {
            if remove.value == add.value {
                new_removes.remove(remove_index);
                add_index -= 1;
                new_adds.remove(add_index);
                break;
            }
        }
    }

    // TODO: We should probably remove duplicate entries here too. So if we have
    // {+A+A}, that would become just {+A}. Similarly {+B-A+B} would be just
    // {+B-A}.

    Ok(Conflict {
        adds: new_adds,
        removes: new_removes,
    })
}
