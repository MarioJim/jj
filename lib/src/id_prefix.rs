// Copyright 2023 The Jujutsu Authors
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

use std::rc::Rc;

use once_cell::unsync::OnceCell;

use crate::backend::{self, ChangeId, CommitId, ObjectId};
use crate::index::{HexPrefix, PrefixResolution};
use crate::op_store::WorkspaceId;
use crate::repo::Repo;
use crate::revset::{DefaultSymbolResolver, RevsetExpression, RevsetIteratorExt};

struct PrefixDisambiguationError;

struct DisambiguationData {
    expression: Rc<RevsetExpression>,
    workspace_id: Option<WorkspaceId>,
    // TODO: We shouldn't have to duplicate the CommitId as value
    indexes: OnceCell<Indexes>,
}

struct Indexes {
    commit_index: IdIndex<CommitId, CommitId>,
    change_index: IdIndex<ChangeId, CommitId>,
}

impl DisambiguationData {
    fn indexes(&self, repo: &dyn Repo) -> Result<&Indexes, PrefixDisambiguationError> {
        self.indexes.get_or_try_init(|| {
            let symbol_resolver = DefaultSymbolResolver::new(repo, self.workspace_id.as_ref());
            let resolved_expression = self
                .expression
                .clone()
                .resolve_user_expression(repo, &symbol_resolver)
                .map_err(|_| PrefixDisambiguationError)?;
            let revset = resolved_expression
                .evaluate(repo)
                .map_err(|_| PrefixDisambiguationError)?;

            // TODO: We should be able to get the change IDs from the revset, without having
            // to read the whole commit objects
            let mut commit_id_vec = vec![];
            let mut change_id_vec = vec![];
            for commit in revset.iter().commits(repo.store()) {
                let commit = commit.map_err(|_| PrefixDisambiguationError)?;
                commit_id_vec.push((commit.id().clone(), commit.id().clone()));
                change_id_vec.push((commit.change_id().clone(), commit.id().clone()));
            }
            Ok(Indexes {
                commit_index: IdIndex::from_vec(commit_id_vec),
                change_index: IdIndex::from_vec(change_id_vec),
            })
        })
    }
}

#[derive(Default)]
pub struct IdPrefixContext {
    disambiguation: Option<DisambiguationData>,
}

impl IdPrefixContext {
    pub fn disambiguate_within(
        mut self,
        expression: Rc<RevsetExpression>,
        workspace_id: Option<WorkspaceId>,
    ) -> Self {
        self.disambiguation = Some(DisambiguationData {
            workspace_id,
            expression,
            indexes: OnceCell::new(),
        });
        self
    }

    fn disambiguation_indexes(&self, repo: &dyn Repo) -> Option<&Indexes> {
        // TODO: propagate errors instead of treating them as if no revset was specified
        self.disambiguation
            .as_ref()
            .and_then(|disambiguation| disambiguation.indexes(repo).ok())
    }

    /// Resolve an unambiguous commit ID prefix.
    pub fn resolve_commit_prefix(
        &self,
        repo: &dyn Repo,
        prefix: &HexPrefix,
    ) -> PrefixResolution<CommitId> {
        if let Some(indexes) = self.disambiguation_indexes(repo) {
            let resolution = indexes.commit_index.resolve_prefix(prefix);
            if let PrefixResolution::SingleMatch(mut ids) = resolution {
                assert_eq!(ids.len(), 1);
                return PrefixResolution::SingleMatch(ids.pop().unwrap());
            }
        }
        repo.index().resolve_prefix(prefix)
    }

    /// Returns the shortest length of a prefix of `commit_id` that
    /// can still be resolved by `resolve_commit_prefix()`.
    pub fn shortest_commit_prefix_len(&self, repo: &dyn Repo, commit_id: &CommitId) -> usize {
        if let Some(indexes) = self.disambiguation_indexes(repo) {
            // TODO: Avoid the double lookup here (has_key() + shortest_unique_prefix_len())
            if indexes.commit_index.has_key(commit_id) {
                return indexes.commit_index.shortest_unique_prefix_len(commit_id);
            }
        }
        repo.index().shortest_unique_commit_id_prefix_len(commit_id)
    }

    /// Resolve an unambiguous change ID prefix to the commit IDs in the revset.
    pub fn resolve_change_prefix(
        &self,
        repo: &dyn Repo,
        prefix: &HexPrefix,
    ) -> PrefixResolution<Vec<CommitId>> {
        if let Some(indexes) = self.disambiguation_indexes(repo) {
            let resolution = indexes.change_index.resolve_prefix(prefix);
            if let PrefixResolution::SingleMatch(ids) = resolution {
                return PrefixResolution::SingleMatch(ids);
            }
        }
        repo.resolve_change_id_prefix(prefix)
    }

    /// Returns the shortest length of a prefix of `change_id` that
    /// can still be resolved by `resolve_change_prefix()`.
    pub fn shortest_change_prefix_len(&self, repo: &dyn Repo, change_id: &ChangeId) -> usize {
        if let Some(indexes) = self.disambiguation_indexes(repo) {
            if indexes.change_index.has_key(change_id) {
                return indexes.change_index.shortest_unique_prefix_len(change_id);
            }
        }
        repo.shortest_unique_change_id_prefix_len(change_id)
    }
}

#[derive(Debug, Clone)]
pub struct IdIndex<K, V>(Vec<(K, V)>);

impl<K, V> IdIndex<K, V>
where
    K: ObjectId + Ord,
{
    /// Creates new index from the given entries. Multiple values can be
    /// associated with a single key.
    pub fn from_vec(mut vec: Vec<(K, V)>) -> Self {
        vec.sort_unstable_by(|(k0, _), (k1, _)| k0.cmp(k1));
        IdIndex(vec)
    }

    /// Looks up entries with the given prefix, and collects values if matched
    /// entries have unambiguous keys.
    pub fn resolve_prefix_with<U>(
        &self,
        prefix: &HexPrefix,
        mut value_mapper: impl FnMut(&V) -> U,
    ) -> PrefixResolution<Vec<U>> {
        if prefix.min_prefix_bytes().is_empty() {
            // We consider an empty prefix ambiguous even if the index has a single entry.
            return PrefixResolution::AmbiguousMatch;
        }
        let mut range = self.resolve_prefix_range(prefix).peekable();
        if let Some((first_key, _)) = range.peek().copied() {
            let maybe_entries: Option<Vec<_>> = range
                .map(|(k, v)| (k == first_key).then(|| value_mapper(v)))
                .collect();
            if let Some(entries) = maybe_entries {
                PrefixResolution::SingleMatch(entries)
            } else {
                PrefixResolution::AmbiguousMatch
            }
        } else {
            PrefixResolution::NoMatch
        }
    }

    /// Looks up entries with the given prefix, and collects values if matched
    /// entries have unambiguous keys.
    pub fn resolve_prefix(&self, prefix: &HexPrefix) -> PrefixResolution<Vec<V>>
    where
        V: Clone,
    {
        self.resolve_prefix_with(prefix, |v: &V| v.clone())
    }

    /// Iterates over entries with the given prefix.
    pub fn resolve_prefix_range<'a: 'b, 'b>(
        &'a self,
        prefix: &'b HexPrefix,
    ) -> impl Iterator<Item = (&'a K, &'a V)> + 'b {
        let min_bytes = prefix.min_prefix_bytes();
        let pos = self.0.partition_point(|(k, _)| k.as_bytes() < min_bytes);
        self.0[pos..]
            .iter()
            .take_while(|(k, _)| prefix.matches(k))
            .map(|(k, v)| (k, v))
    }

    pub fn has_key(&self, key: &K) -> bool {
        self.0.binary_search_by(|(k, _)| k.cmp(key)).is_ok()
    }

    /// This function returns the shortest length of a prefix of `key` that
    /// disambiguates it from every other key in the index.
    ///
    /// The length to be returned is a number of hexadecimal digits.
    ///
    /// This has some properties that we do not currently make much use of:
    ///
    /// - The algorithm works even if `key` itself is not in the index.
    ///
    /// - In the special case when there are keys in the trie for which our
    ///   `key` is an exact prefix, returns `key.len() + 1`. Conceptually, in
    ///   order to disambiguate, you need every letter of the key *and* the
    ///   additional fact that it's the entire key). This case is extremely
    ///   unlikely for hashes with 12+ hexadecimal characters.
    pub fn shortest_unique_prefix_len(&self, key: &K) -> usize {
        let pos = self.0.partition_point(|(k, _)| k < key);
        let left = pos.checked_sub(1).map(|p| &self.0[p]);
        let right = self.0[pos..].iter().find(|(k, _)| k != key);
        itertools::chain(left, right)
            .map(|(neighbor, _value)| {
                backend::common_hex_len(key.as_bytes(), neighbor.as_bytes()) + 1
            })
            .max()
            // Even if the key is the only one in the index, we require at least one digit.
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ChangeId, ObjectId};

    #[test]
    fn test_id_index_resolve_prefix() {
        fn sorted(resolution: PrefixResolution<Vec<i32>>) -> PrefixResolution<Vec<i32>> {
            match resolution {
                PrefixResolution::SingleMatch(mut xs) => {
                    xs.sort(); // order of values might not be preserved by IdIndex
                    PrefixResolution::SingleMatch(xs)
                }
                _ => resolution,
            }
        }
        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("0000"), 0),
            (ChangeId::from_hex("0099"), 1),
            (ChangeId::from_hex("0099"), 2),
            (ChangeId::from_hex("0aaa"), 3),
            (ChangeId::from_hex("0aab"), 4),
        ]);
        assert_eq!(
            id_index.resolve_prefix(&HexPrefix::new("0").unwrap()),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix(&HexPrefix::new("00").unwrap()),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix(&HexPrefix::new("000").unwrap()),
            PrefixResolution::SingleMatch(vec![0]),
        );
        assert_eq!(
            id_index.resolve_prefix(&HexPrefix::new("0001").unwrap()),
            PrefixResolution::NoMatch,
        );
        assert_eq!(
            sorted(id_index.resolve_prefix(&HexPrefix::new("009").unwrap())),
            PrefixResolution::SingleMatch(vec![1, 2]),
        );
        assert_eq!(
            id_index.resolve_prefix(&HexPrefix::new("0aa").unwrap()),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix(&HexPrefix::new("0aab").unwrap()),
            PrefixResolution::SingleMatch(vec![4]),
        );
        assert_eq!(
            id_index.resolve_prefix(&HexPrefix::new("f").unwrap()),
            PrefixResolution::NoMatch,
        );
    }

    #[test]
    fn test_has_key() {
        // No crash if empty
        let id_index = IdIndex::from_vec(vec![] as Vec<(ChangeId, ())>);
        assert!(!id_index.has_key(&ChangeId::from_hex("00")));

        let id_index = IdIndex::from_vec(vec![(ChangeId::from_hex("ab"), ())]);
        assert!(!id_index.has_key(&ChangeId::from_hex("aa")));
        assert!(id_index.has_key(&ChangeId::from_hex("ab")));
        assert!(!id_index.has_key(&ChangeId::from_hex("ac")));
    }

    #[test]
    fn test_id_index_shortest_unique_prefix_len() {
        // No crash if empty
        let id_index = IdIndex::from_vec(vec![] as Vec<(ChangeId, ())>);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("00")),
            1
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acd0"), ()), // duplicated key is allowed
        ]);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ac")),
            3
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acf0"), ()),
            (ChangeId::from_hex("a0"), ()),
            (ChangeId::from_hex("ba"), ()),
        ]);

        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("a0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ba")),
            1
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ab")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            3
        );
        // If it were there, the length would be 1.
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("c0")),
            1
        );
    }
}
