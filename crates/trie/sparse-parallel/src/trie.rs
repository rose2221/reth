use alloy_primitives::{
    map::{Entry, HashMap},
    B256,
};
use alloy_rlp::Decodable;
use alloy_trie::{BranchNodeCompact, TrieMask, EMPTY_ROOT_HASH};
use reth_execution_errors::{SparseTrieErrorKind, SparseTrieResult};
use reth_trie_common::{
    prefix_set::{PrefixSet, PrefixSetMut},
    BranchNodeRef, ExtensionNodeRef, LeafNodeRef, Nibbles, RlpNode, TrieNode, CHILD_INDEX_RANGE,
};
use reth_trie_sparse::{
    blinded::{BlindedProvider, RevealedNode},
    RlpNodeStackItem, SparseNode, SparseNodeType, SparseTrieUpdates, TrieMasks,
};
use smallvec::SmallVec;
use std::sync::mpsc;
use tracing::{instrument, trace};

/// The maximum length of a path, in nibbles, which belongs to the upper subtrie of a
/// [`ParallelSparseTrie`]. All longer paths belong to a lower subtrie.
pub const UPPER_TRIE_MAX_DEPTH: usize = 2;

/// Number of lower subtries which are managed by the [`ParallelSparseTrie`].
pub const NUM_LOWER_SUBTRIES: usize = 16usize.pow(UPPER_TRIE_MAX_DEPTH as u32);

/// A revealed sparse trie with subtries that can be updated in parallel.
///
/// ## Invariants
///
/// - Each leaf entry in the `subtries` and `upper_trie` collection must have a corresponding entry
///   in `values` collection. If the root node is a leaf, it must also have an entry in `values`.
/// - All keys in `values` collection are full leaf paths.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParallelSparseTrie {
    /// This contains the trie nodes for the upper part of the trie.
    upper_subtrie: Box<SparseSubtrie>,
    /// An array containing the subtries at the second level of the trie.
    lower_subtries: [Option<Box<SparseSubtrie>>; NUM_LOWER_SUBTRIES],
    /// Set of prefixes (key paths) that have been marked as updated.
    /// This is used to track which parts of the trie need to be recalculated.
    prefix_set: PrefixSetMut,
    /// Optional tracking of trie updates for later use.
    updates: Option<SparseTrieUpdates>,
}

impl Default for ParallelSparseTrie {
    fn default() -> Self {
        Self {
            upper_subtrie: Box::default(),
            lower_subtries: [const { None }; NUM_LOWER_SUBTRIES],
            prefix_set: PrefixSetMut::default(),
            updates: None,
        }
    }
}

impl ParallelSparseTrie {
    /// Returns a mutable reference to the lower `SparseSubtrie` for the given path, or None if the
    /// path belongs to the upper trie.
    ///
    /// This method will create a new lower subtrie if one doesn't exist for the given path.
    fn lower_subtrie_for_path(&mut self, path: &Nibbles) -> Option<&mut Box<SparseSubtrie>> {
        match SparseSubtrieType::from_path(path) {
            SparseSubtrieType::Upper => None,
            SparseSubtrieType::Lower(idx) => {
                if self.lower_subtries[idx].is_none() {
                    let upper_path = path.slice(..UPPER_TRIE_MAX_DEPTH);
                    self.lower_subtries[idx] = Some(Box::new(SparseSubtrie::new(upper_path)));
                }

                self.lower_subtries[idx].as_mut()
            }
        }
    }

    /// Returns a mutable reference to either the lower or upper `SparseSubtrie` for the given path,
    /// depending on the path's length.
    ///
    /// This method will create a new lower subtrie if one doesn't exist for the given path.
    fn subtrie_for_path(&mut self, path: &Nibbles) -> &mut Box<SparseSubtrie> {
        match SparseSubtrieType::from_path(path) {
            SparseSubtrieType::Upper => &mut self.upper_subtrie,
            SparseSubtrieType::Lower(idx) => {
                if self.lower_subtries[idx].is_none() {
                    let upper_path = path.slice(..UPPER_TRIE_MAX_DEPTH);
                    self.lower_subtries[idx] = Some(Box::new(SparseSubtrie::new(upper_path)));
                }

                self.lower_subtries[idx].as_mut().unwrap()
            }
        }
    }

    /// Creates a new revealed sparse trie from the given root node.
    ///
    /// # Returns
    ///
    /// A [`ParallelSparseTrie`] if successful, or an error if revealing fails.
    pub fn from_root(
        root_node: TrieNode,
        masks: TrieMasks,
        retain_updates: bool,
    ) -> SparseTrieResult<Self> {
        let mut trie = Self::default().with_updates(retain_updates);
        trie.reveal_node(Nibbles::default(), root_node, masks)?;
        Ok(trie)
    }

    /// Reveals a trie node if it has not been revealed before.
    ///
    /// This internal function decodes a trie node and inserts it into the nodes map.
    /// It handles different node types (leaf, extension, branch) by appropriately
    /// adding them to the trie structure and recursively revealing their children.
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, or an error if node was not revealed.
    pub fn reveal_node(
        &mut self,
        path: Nibbles,
        node: TrieNode,
        masks: TrieMasks,
    ) -> SparseTrieResult<()> {
        if let Some(subtrie) = self.lower_subtrie_for_path(&path) {
            return subtrie.reveal_node(path, &node, masks);
        }

        // If there is no subtrie for the path it means the path is UPPER_TRIE_MAX_DEPTH or less
        // nibbles, and so belongs to the upper trie.
        self.upper_subtrie.reveal_node(path, &node, masks)?;

        // The previous upper_trie.reveal_node call will not have revealed any child nodes via
        // reveal_node_or_hash if the child node would be found on a lower subtrie. We handle that
        // here by manually checking the specific cases where this could happen, and calling
        // reveal_node_or_hash for each.
        match node {
            TrieNode::Branch(branch) => {
                // If a branch is at the cutoff level of the trie then it will be in the upper trie,
                // but all of its children will be in a lower trie. Check if a child node would be
                // in the lower subtrie, and reveal accordingly.
                if !SparseSubtrieType::path_len_is_upper(path.len() + 1) {
                    let mut stack_ptr = branch.as_ref().first_child_index();
                    for idx in CHILD_INDEX_RANGE {
                        if branch.state_mask.is_bit_set(idx) {
                            let mut child_path = path;
                            child_path.push_unchecked(idx);
                            self.lower_subtrie_for_path(&child_path)
                                .expect("child_path must have a lower subtrie")
                                .reveal_node_or_hash(child_path, &branch.stack[stack_ptr])?;
                            stack_ptr += 1;
                        }
                    }
                }
            }
            TrieNode::Extension(ext) => {
                let mut child_path = path;
                child_path.extend(&ext.key);
                if let Some(subtrie) = self.lower_subtrie_for_path(&child_path) {
                    subtrie.reveal_node_or_hash(child_path, &ext.child)?;
                }
            }
            TrieNode::EmptyRoot | TrieNode::Leaf(_) => (),
        }

        Ok(())
    }

    /// Updates or inserts a leaf node at the specified key path with the provided RLP-encoded
    /// value.
    ///
    /// This method updates the internal prefix set and, if the leaf did not previously exist,
    /// adjusts the trie structure by inserting new leaf nodes, splitting branch nodes, or
    /// collapsing extension nodes as needed.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the update is successful.
    ///
    /// Note: If an update requires revealing a blinded node, an error is returned if the blinded
    /// provider returns an error.
    pub fn update_leaf(
        &mut self,
        key_path: Nibbles,
        value: Vec<u8>,
        masks: TrieMasks,
        provider: impl BlindedProvider,
    ) -> SparseTrieResult<()> {
        let _key_path = key_path;
        let _value = value;
        let _masks = masks;
        let _provider = provider;
        todo!()
    }

    /// Returns the next node in the traversal path from the given path towards the leaf for the
    /// given full leaf path, or an error if any node along the traversal path is not revealed.
    ///
    ///
    /// ## Panics
    ///
    /// If `from_path` is not a prefix of `leaf_full_path`.
    fn find_next_to_leaf(
        from_path: &Nibbles,
        from_node: &SparseNode,
        leaf_full_path: &Nibbles,
    ) -> SparseTrieResult<FindNextToLeafOutcome> {
        debug_assert!(leaf_full_path.len() >= from_path.len());
        debug_assert!(leaf_full_path.starts_with(from_path));

        match from_node {
            SparseNode::Empty => Err(SparseTrieErrorKind::Blind.into()),
            SparseNode::Hash(hash) => {
                Err(SparseTrieErrorKind::BlindedNode { path: *from_path, hash: *hash }.into())
            }
            SparseNode::Leaf { key, .. } => {
                let mut found_full_path = *from_path;
                found_full_path.extend(key);

                if &found_full_path == leaf_full_path {
                    return Ok(FindNextToLeafOutcome::Found)
                }
                Ok(FindNextToLeafOutcome::NotFound)
            }
            SparseNode::Extension { key, .. } => {
                if leaf_full_path.len() == from_path.len() {
                    return Ok(FindNextToLeafOutcome::NotFound)
                }

                let mut child_path = *from_path;
                child_path.extend(key);

                if !leaf_full_path.starts_with(&child_path) {
                    return Ok(FindNextToLeafOutcome::NotFound)
                }
                Ok(FindNextToLeafOutcome::ContinueFrom(child_path))
            }
            SparseNode::Branch { state_mask, .. } => {
                if leaf_full_path.len() == from_path.len() {
                    return Ok(FindNextToLeafOutcome::NotFound)
                }

                let nibble = leaf_full_path.get_unchecked(from_path.len());
                if !state_mask.is_bit_set(nibble) {
                    return Ok(FindNextToLeafOutcome::NotFound)
                }

                let mut child_path = *from_path;
                child_path.push_unchecked(nibble);

                Ok(FindNextToLeafOutcome::ContinueFrom(child_path))
            }
        }
    }

    /// Called when a child node has collapsed into its parent as part of `remove_leaf`. If the
    /// new parent node is a leaf, then the previous child also was, and if the previous child was
    /// on a lower subtrie while the parent is on an upper then the leaf value needs to be moved to
    /// the upper.
    fn move_value_on_leaf_removal(
        &mut self,
        parent_path: &Nibbles,
        new_parent_node: &SparseNode,
        prev_child_path: &Nibbles,
    ) {
        // If the parent path isn't in the upper then it doesn't matter what the new node is,
        // there's no situation where a leaf value needs to be moved.
        if SparseSubtrieType::from_path(parent_path).lower_index().is_some() {
            return;
        }

        if let SparseNode::Leaf { key, .. } = new_parent_node {
            let Some(prev_child_subtrie) = self.lower_subtrie_for_path(prev_child_path) else {
                return;
            };

            let mut leaf_full_path = *parent_path;
            leaf_full_path.extend(key);

            let val = prev_child_subtrie.inner.values.remove(&leaf_full_path).expect("ParallelSparseTrie is in an inconsistent state, expected value on subtrie which wasn't found");
            self.upper_subtrie.inner.values.insert(leaf_full_path, val);
        }
    }

    /// Given the path to a parent branch node and a child node which is the sole remaining child on
    /// that branch after removing a leaf, returns a node to replace the parent branch node and a
    /// boolean indicating if the child should be deleted.
    ///
    /// ## Panics
    ///
    /// - If either parent or child node is not already revealed.
    /// - If parent's path is not a prefix of the child's path.
    fn branch_changes_on_leaf_removal(
        parent_path: &Nibbles,
        remaining_child_path: &Nibbles,
        remaining_child_node: &SparseNode,
    ) -> (SparseNode, bool) {
        debug_assert!(remaining_child_path.len() > parent_path.len());
        debug_assert!(remaining_child_path.starts_with(parent_path));

        let remaining_child_nibble = remaining_child_path.get_unchecked(parent_path.len());

        // If we swap the branch node out either an extension or leaf, depending on
        // what its remaining child is.
        match remaining_child_node {
            SparseNode::Empty | SparseNode::Hash(_) => {
                panic!("remaining child must have been revealed already")
            }
            // If the only child is a leaf node, we downgrade the branch node into a
            // leaf node, prepending the nibble to the key, and delete the old
            // child.
            SparseNode::Leaf { key, .. } => {
                let mut new_key = Nibbles::from_nibbles_unchecked([remaining_child_nibble]);
                new_key.extend(key);
                (SparseNode::new_leaf(new_key), true)
            }
            // If the only child node is an extension node, we downgrade the branch
            // node into an even longer extension node, prepending the nibble to the
            // key, and delete the old child.
            SparseNode::Extension { key, .. } => {
                let mut new_key = Nibbles::from_nibbles_unchecked([remaining_child_nibble]);
                new_key.extend(key);
                (SparseNode::new_ext(new_key), true)
            }
            // If the only child is a branch node, we downgrade the current branch
            // node into a one-nibble extension node.
            SparseNode::Branch { .. } => (
                SparseNode::new_ext(Nibbles::from_nibbles_unchecked([remaining_child_nibble])),
                false,
            ),
        }
    }

    /// Given the path to a parent extension and its key, and a child node (not necessarily on this
    /// subtrie), returns an optional replacement parent node. If a replacement is returned then the
    /// child node should be deleted.
    ///
    /// ## Panics
    ///
    /// - If either parent or child node is not already revealed.
    /// - If parent's path is not a prefix of the child's path.
    fn extension_changes_on_leaf_removal(
        parent_path: &Nibbles,
        parent_key: &Nibbles,
        child_path: &Nibbles,
        child: &SparseNode,
    ) -> Option<SparseNode> {
        debug_assert!(child_path.len() > parent_path.len());
        debug_assert!(child_path.starts_with(parent_path));

        // If the parent node is an extension node, we need to look at its child to see
        // if we need to merge it.
        match child {
            SparseNode::Empty | SparseNode::Hash(_) => {
                panic!("child must be revealed")
            }
            // For a leaf node, we collapse the extension node into a leaf node,
            // extending the key. While it's impossible to encounter an extension node
            // followed by a leaf node in a complete trie, it's possible here because we
            // could have downgraded the extension node's child into a leaf node from a
            // branch in a previous call to `branch_changes_on_leaf_removal`.
            SparseNode::Leaf { key, .. } => {
                let mut new_key = *parent_key;
                new_key.extend(key);
                Some(SparseNode::new_leaf(new_key))
            }
            // Similar to the leaf node, for an extension node, we collapse them into one
            // extension node, extending the key.
            SparseNode::Extension { key, .. } => {
                let mut new_key = *parent_key;
                new_key.extend(key);
                Some(SparseNode::new_ext(new_key))
            }
            // For a branch node, we just leave the extension node as-is.
            SparseNode::Branch { .. } => None,
        }
    }

    /// Removes a leaf node from the trie at the specified full path of a value (that is, the leaf's
    /// path + its key).
    ///
    /// This function removes the leaf value from the internal values map and then traverses
    /// the trie to remove or adjust intermediate nodes, merging or collapsing them as necessary.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the leaf is successfully removed or was not present in the trie,
    /// otherwise returns an error if a blinded node prevents removal.
    pub fn remove_leaf(
        &mut self,
        leaf_full_path: &Nibbles,
        provider: impl BlindedProvider,
    ) -> SparseTrieResult<()> {
        // When removing a leaf node it's possibly necessary to modify its parent node, and possibly
        // the parent's parent node. It is not ever necessary to descend further than that; once an
        // extension node is hit it must terminate in a branch or the root, which won't need further
        // updates. So the situation with maximum updates is:
        //
        // - Leaf
        // - Branch with 2 children, one being this leaf
        // - Extension
        //
        // ...which will result in just a leaf or extension, depending on what the branch's other
        // child is.
        //
        // Therefore, first traverse the trie in order to find the leaf node and at most its parent
        // and grandparent.

        let leaf_path;
        let leaf_subtrie;

        let mut branch_parent_path: Option<Nibbles> = None;
        let mut branch_parent_node: Option<SparseNode> = None;

        let mut ext_grandparent_path: Option<Nibbles> = None;
        let mut ext_grandparent_node: Option<SparseNode> = None;

        let mut curr_path = Nibbles::new(); // start traversal from root
        let mut curr_subtrie = self.upper_subtrie.as_mut();
        let mut curr_subtrie_is_upper = true;

        loop {
            let curr_node = curr_subtrie.nodes.get_mut(&curr_path).unwrap();

            match Self::find_next_to_leaf(&curr_path, curr_node, leaf_full_path)? {
                FindNextToLeafOutcome::NotFound => return Ok(()), // leaf isn't in the trie
                FindNextToLeafOutcome::Found => {
                    // this node is the target leaf
                    leaf_path = curr_path;
                    leaf_subtrie = curr_subtrie;
                    break;
                }
                FindNextToLeafOutcome::ContinueFrom(next_path) => {
                    // Any branches/extensions along the path to the leaf will have their `hash`
                    // field unset, as it will no longer be valid once the leaf is removed.
                    match curr_node {
                        SparseNode::Branch { hash, .. } => {
                            *hash = None;

                            // If there is already an extension leading into a branch, then that
                            // extension is no longer relevant.
                            match (&branch_parent_path, &ext_grandparent_path) {
                                (Some(branch), Some(ext)) if branch.len() > ext.len() => {
                                    ext_grandparent_path = None;
                                    ext_grandparent_node = None;
                                }
                                _ => (),
                            };
                            branch_parent_path = Some(curr_path);
                            branch_parent_node = Some(curr_node.clone());
                        }
                        SparseNode::Extension { hash, .. } => {
                            *hash = None;

                            // We can assume a new branch node will be found after the extension, so
                            // there's no need to modify branch_parent_path/node even if it's
                            // already set.
                            ext_grandparent_path = Some(curr_path);
                            ext_grandparent_node = Some(curr_node.clone());
                        }
                        SparseNode::Empty | SparseNode::Hash(_) | SparseNode::Leaf { .. } => {
                            unreachable!("find_next_to_leaf errors on non-revealed node, and return Found or NotFound on Leaf")
                        }
                    }

                    curr_path = next_path;

                    // If we were previously looking at the upper trie, and the new path is in the
                    // lower trie, we need to pull out a ref to the lower trie.
                    if curr_subtrie_is_upper {
                        if let SparseSubtrieType::Lower(idx) =
                            SparseSubtrieType::from_path(&curr_path)
                        {
                            curr_subtrie = self.lower_subtries[idx].as_mut().unwrap();
                            curr_subtrie_is_upper = false;
                        }
                    }
                }
            };
        }

        // We've traversed to the leaf and collected its ancestors as necessary. Remove the leaf
        // from its SparseSubtrie.
        self.prefix_set.insert(*leaf_full_path);
        leaf_subtrie.inner.values.remove(leaf_full_path);
        leaf_subtrie.nodes.remove(&leaf_path);

        // If the leaf was at the root replace its node with the empty value. We can stop execution
        // here, all remaining logic is related to the ancestors of the leaf.
        if leaf_path.is_empty() {
            self.upper_subtrie.nodes.insert(leaf_path, SparseNode::Empty);
            return Ok(())
        }

        // If there is a parent branch node (very likely, unless the leaf is at the root) execute
        // any required changes for that node, relative to the removed leaf.
        if let (Some(branch_path), Some(SparseNode::Branch { mut state_mask, .. })) =
            (&branch_parent_path, &branch_parent_node)
        {
            let child_nibble = leaf_path.get_unchecked(branch_path.len());
            state_mask.unset_bit(child_nibble);

            let new_branch_node = if state_mask.count_bits() == 1 {
                // If only one child is left set in the branch node, we need to collapse it. Get
                // full path of the only child node left.
                let remaining_child_path = {
                    let mut p = *branch_path;
                    p.push_unchecked(
                        state_mask.first_set_bit_index().expect("state mask is not empty"),
                    );
                    p
                };

                trace!(
                    target: "trie::parallel_sparse",
                    ?leaf_path,
                    ?branch_path,
                    ?remaining_child_path,
                    "Branch node has only one child",
                );

                let remaining_child_subtrie = self.subtrie_for_path(&remaining_child_path);

                // If the remaining child node is not yet revealed then we have to reveal it here,
                // otherwise it's not possible to know how to collapse the branch.
                let remaining_child_node =
                    match remaining_child_subtrie.nodes.get(&remaining_child_path).unwrap() {
                        SparseNode::Hash(_) => {
                            trace!(
                                target: "trie::parallel_sparse",
                                ?remaining_child_path,
                                "Retrieving remaining blinded branch child",
                            );
                            if let Some(RevealedNode { node, tree_mask, hash_mask }) =
                                provider.blinded_node(&remaining_child_path)?
                            {
                                let decoded = TrieNode::decode(&mut &node[..])?;
                                trace!(
                                    target: "trie::parallel_sparse",
                                    ?remaining_child_path,
                                    ?decoded,
                                    ?tree_mask,
                                    ?hash_mask,
                                    "Revealing remaining blinded branch child"
                                );
                                remaining_child_subtrie.reveal_node(
                                    remaining_child_path,
                                    &decoded,
                                    TrieMasks { hash_mask, tree_mask },
                                )?;
                                remaining_child_subtrie.nodes.get(&remaining_child_path).unwrap()
                            } else {
                                return Err(SparseTrieErrorKind::NodeNotFoundInProvider {
                                    path: remaining_child_path,
                                }
                                .into())
                            }
                        }
                        node => node,
                    };

                let (new_branch_node, remove_child) = Self::branch_changes_on_leaf_removal(
                    branch_path,
                    &remaining_child_path,
                    remaining_child_node,
                );

                if remove_child {
                    remaining_child_subtrie.nodes.remove(&remaining_child_path);
                    self.move_value_on_leaf_removal(
                        branch_path,
                        &new_branch_node,
                        &remaining_child_path,
                    );
                }

                if let Some(updates) = self.updates.as_mut() {
                    updates.updated_nodes.remove(branch_path);
                    updates.removed_nodes.insert(*branch_path);
                }

                new_branch_node
            } else {
                // If more than one child is left set in the branch, we just re-insert it with the
                // updated state_mask.
                SparseNode::new_branch(state_mask)
            };

            let branch_subtrie = self.subtrie_for_path(branch_path);
            branch_subtrie.nodes.insert(*branch_path, new_branch_node.clone());
            branch_parent_node = Some(new_branch_node);
        };

        // If there is a grandparent extension node then there will necessarily be a parent branch
        // node. Execute any required changes for the extension node, relative to the (possibly now
        // replaced with a leaf or extension) branch node.
        if let (Some(ext_path), Some(SparseNode::Extension { key: shortkey, .. })) =
            (ext_grandparent_path, &ext_grandparent_node)
        {
            let ext_subtrie = self.subtrie_for_path(&ext_path);
            let branch_path = branch_parent_path.as_ref().unwrap();

            if let Some(new_ext_node) = Self::extension_changes_on_leaf_removal(
                &ext_path,
                shortkey,
                branch_path,
                branch_parent_node.as_ref().unwrap(),
            ) {
                ext_subtrie.nodes.insert(ext_path, new_ext_node.clone());
                self.subtrie_for_path(branch_path).nodes.remove(branch_path);
                self.move_value_on_leaf_removal(&ext_path, &new_ext_node, branch_path);
            }
        }

        Ok(())
    }

    /// Recalculates and updates the RLP hashes of nodes up to level [`UPPER_TRIE_MAX_DEPTH`] of the
    /// trie.
    ///
    /// The root node is considered to be at level 0. This method is useful for optimizing
    /// hash recalculations after localized changes to the trie structure.
    ///
    /// This function first identifies all nodes that have changed (based on the prefix set) below
    /// level [`UPPER_TRIE_MAX_DEPTH`] of the trie, then recalculates their RLP representation.
    pub fn update_lower_subtrie_hashes(&mut self) {
        trace!(target: "trie::parallel_sparse", "Updating subtrie hashes");

        // Take changed subtries according to the prefix set
        let mut prefix_set = core::mem::take(&mut self.prefix_set).freeze();
        let (subtries, unchanged_prefix_set) = self.take_changed_lower_subtries(&mut prefix_set);

        // Update the prefix set with the keys that didn't have matching subtries
        self.prefix_set = unchanged_prefix_set;

        // Update subtrie hashes in parallel
        // TODO: call `update_hashes` on each subtrie in parallel
        let (tx, rx) = mpsc::channel();
        for ChangedSubtrie { index, mut subtrie, mut prefix_set } in subtries {
            subtrie.update_hashes(&mut prefix_set);
            tx.send((index, subtrie)).unwrap();
        }
        drop(tx);

        // Return updated subtries back to the trie
        for (index, subtrie) in rx {
            self.lower_subtries[index] = Some(subtrie);
        }
    }

    /// Updates hashes for the upper subtrie, using nodes from both upper and lower subtries.
    #[instrument(level = "trace", target = "engine::tree", skip_all, ret)]
    fn update_upper_subtrie_hashes(&mut self, prefix_set: &mut PrefixSet) -> RlpNode {
        trace!(target: "trie::parallel_sparse", "Updating upper subtrie hashes");

        debug_assert!(self.upper_subtrie.inner.buffers.path_stack.is_empty());
        self.upper_subtrie.inner.buffers.path_stack.push(RlpNodePathStackItem {
            path: Nibbles::default(), // Start from root
            is_in_prefix_set: None,
        });

        while let Some(stack_item) = self.upper_subtrie.inner.buffers.path_stack.pop() {
            let path = stack_item.path;
            let node = if path.len() < UPPER_TRIE_MAX_DEPTH {
                self.upper_subtrie.nodes.get_mut(&path).expect("upper subtrie node must exist")
            } else {
                let index = path_subtrie_index_unchecked(&path);
                let node = self.lower_subtries[index]
                    .as_mut()
                    .expect("lower subtrie must exist")
                    .nodes
                    .get_mut(&path)
                    .expect("lower subtrie node must exist");
                // Lower subtrie root node hashes must be computed before updating upper subtrie
                // hashes
                debug_assert!(node.hash().is_some());
                node
            };

            // Calculate the RLP node for the current node using upper subtrie
            self.upper_subtrie.inner.rlp_node(prefix_set, stack_item, node);
        }

        debug_assert_eq!(self.upper_subtrie.inner.buffers.rlp_node_stack.len(), 1);
        self.upper_subtrie.inner.buffers.rlp_node_stack.pop().unwrap().rlp_node
    }

    /// Calculates and returns the root hash of the trie.
    ///
    /// Before computing the hash, this function processes any remaining (dirty) nodes by
    /// updating their RLP encodings. The root hash is either:
    /// 1. The cached hash (if no dirty nodes were found)
    /// 2. The keccak256 hash of the root node's RLP representation
    pub fn root(&mut self) -> B256 {
        trace!(target: "trie::parallel_sparse", "Calculating trie root hash");

        // Update all lower subtrie hashes
        self.update_lower_subtrie_hashes();

        // Update hashes for the upper subtrie using our specialized function
        // that can access both upper and lower subtrie nodes
        let mut prefix_set = core::mem::take(&mut self.prefix_set).freeze();
        let root_rlp = self.update_upper_subtrie_hashes(&mut prefix_set);

        // Return the root hash
        root_rlp.as_hash().unwrap_or(EMPTY_ROOT_HASH)
    }

    /// Configures the trie to retain information about updates.
    ///
    /// If `retain_updates` is true, the trie will record branch node updates and deletions.
    /// This information can then be used to efficiently update an external database.
    pub fn with_updates(mut self, retain_updates: bool) -> Self {
        self.updates = retain_updates.then_some(SparseTrieUpdates::default());
        self
    }

    /// Consumes and returns the currently accumulated trie updates.
    ///
    /// This is useful when you want to apply the updates to an external database,
    /// and then start tracking a new set of updates.
    pub fn take_updates(&mut self) -> SparseTrieUpdates {
        core::iter::once(&mut self.upper_subtrie)
            .chain(self.lower_subtries.iter_mut().flatten())
            .fold(SparseTrieUpdates::default(), |mut acc, subtrie| {
                acc.extend(subtrie.take_updates());
                acc
            })
    }

    /// Returns:
    /// 1. List of lower [subtries](SparseSubtrie) that have changed according to the provided
    ///    [prefix set](PrefixSet). See documentation of [`ChangedSubtrie`] for more details.
    /// 2. Prefix set of keys that do not belong to any lower subtrie.
    ///
    /// This method helps optimize hash recalculations by identifying which specific
    /// lower subtries need to be updated. Each lower subtrie can then be updated in parallel.
    ///
    /// IMPORTANT: The method removes the subtries from `lower_subtries`, and the caller is
    /// responsible for returning them back into the array.
    fn take_changed_lower_subtries(
        &mut self,
        prefix_set: &mut PrefixSet,
    ) -> (Vec<ChangedSubtrie>, PrefixSetMut) {
        // Clone the prefix set to iterate over its keys. Cloning is cheap, it's just an Arc.
        let prefix_set_clone = prefix_set.clone();
        let mut prefix_set_iter = prefix_set_clone.into_iter().copied().peekable();
        let mut changed_subtries = Vec::new();
        let mut unchanged_prefix_set = PrefixSetMut::default();

        for (index, subtrie) in self.lower_subtries.iter_mut().enumerate() {
            if let Some(subtrie) = subtrie.take_if(|subtrie| prefix_set.contains(&subtrie.path)) {
                let prefix_set = if prefix_set.all() {
                    unchanged_prefix_set = PrefixSetMut::all();
                    PrefixSetMut::all()
                } else {
                    // Take those keys from the original prefix set that start with the subtrie path
                    //
                    // Subtries are stored in the order of their paths, so we can use the same
                    // prefix set iterator.
                    let mut new_prefix_set = Vec::new();
                    while let Some(key) = prefix_set_iter.peek() {
                        if key.starts_with(&subtrie.path) {
                            // If the key starts with the subtrie path, add it to the new prefix set
                            new_prefix_set.push(prefix_set_iter.next().unwrap());
                        } else if new_prefix_set.is_empty() && key < &subtrie.path {
                            // If we didn't yet have any keys that belong to this subtrie, and the
                            // current key is still less than the subtrie path, add it to the
                            // unchanged prefix set
                            unchanged_prefix_set.insert(prefix_set_iter.next().unwrap());
                        } else {
                            // If we're past the subtrie path, we're done with this subtrie. Do not
                            // advance the iterator, the next key will be processed either by the
                            // next subtrie or inserted into the unchanged prefix set.
                            break
                        }
                    }
                    PrefixSetMut::from(new_prefix_set)
                }
                .freeze();

                changed_subtries.push(ChangedSubtrie { index, subtrie, prefix_set });
            }
        }

        // Extend the unchanged prefix set with the remaining keys that are not part of any subtries
        unchanged_prefix_set.extend_keys(prefix_set_iter);

        (changed_subtries, unchanged_prefix_set)
    }
}

/// This is a subtrie of the [`ParallelSparseTrie`] that contains a map from path to sparse trie
/// nodes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SparseSubtrie {
    /// The root path of this subtrie.
    ///
    /// This is the _full_ path to this subtrie, meaning it includes the first
    /// [`UPPER_TRIE_MAX_DEPTH`] nibbles that we also use for indexing subtries in the
    /// [`ParallelSparseTrie`].
    ///
    /// There should be a node for this path in `nodes` map.
    path: Nibbles,
    /// The map from paths to sparse trie nodes within this subtrie.
    nodes: HashMap<Nibbles, SparseNode>,
    /// Subset of fields for mutable access while `nodes` field is also being mutably borrowed.
    inner: SparseSubtrieInner,
}

/// Returned by the `find_next_to_leaf` method to indicate either that the leaf has been found,
/// traversal should be continued from the given path, or the leaf is not in the trie.
enum FindNextToLeafOutcome {
    /// `Found` indicates that the leaf was found at the given path.
    Found,
    /// `ContinueFrom` indicates that traversal should continue from the given path.
    ContinueFrom(Nibbles),
    /// `NotFound` indicates that there is no way to traverse to the leaf, as it is not in the
    /// trie.
    NotFound,
}

impl SparseSubtrie {
    fn new(path: Nibbles) -> Self {
        Self { path, ..Default::default() }
    }

    /// Configures the subtrie to retain information about updates.
    ///
    /// If `retain_updates` is true, the trie will record branch node updates and deletions.
    /// This information can then be used to efficiently update an external database.
    pub fn with_updates(mut self, retain_updates: bool) -> Self {
        self.inner.updates = retain_updates.then_some(SparseTrieUpdates::default());
        self
    }

    /// Returns true if the current path and its child are both found in the same level.
    fn is_child_same_level(current_path: &Nibbles, child_path: &Nibbles) -> bool {
        let current_level = core::mem::discriminant(&SparseSubtrieType::from_path(current_path));
        let child_level = core::mem::discriminant(&SparseSubtrieType::from_path(child_path));
        current_level == child_level
    }

    /// Internal implementation of the method of the same name on `ParallelSparseTrie`.
    fn reveal_node(
        &mut self,
        path: Nibbles,
        node: &TrieNode,
        masks: TrieMasks,
    ) -> SparseTrieResult<()> {
        debug_assert!(path.starts_with(&self.path));

        // If the node is already revealed and it's not a hash node, do nothing.
        if self.nodes.get(&path).is_some_and(|node| !node.is_hash()) {
            return Ok(())
        }

        if let Some(tree_mask) = masks.tree_mask {
            self.inner.branch_node_tree_masks.insert(path, tree_mask);
        }
        if let Some(hash_mask) = masks.hash_mask {
            self.inner.branch_node_hash_masks.insert(path, hash_mask);
        }

        match node {
            TrieNode::EmptyRoot => {
                // For an empty root, ensure that we are at the root path, and at the upper subtrie.
                debug_assert!(path.is_empty());
                debug_assert!(self.path.is_empty());
                self.nodes.insert(path, SparseNode::Empty);
            }
            TrieNode::Branch(branch) => {
                // For a branch node, iterate over all potential children
                let mut stack_ptr = branch.as_ref().first_child_index();
                for idx in CHILD_INDEX_RANGE {
                    if branch.state_mask.is_bit_set(idx) {
                        let mut child_path = path;
                        child_path.push_unchecked(idx);
                        if Self::is_child_same_level(&path, &child_path) {
                            // Reveal each child node or hash it has, but only if the child is on
                            // the same level as the parent.
                            self.reveal_node_or_hash(child_path, &branch.stack[stack_ptr])?;
                        }
                        stack_ptr += 1;
                    }
                }
                // Update the branch node entry in the nodes map, handling cases where a blinded
                // node is now replaced with a revealed node.
                match self.nodes.entry(path) {
                    Entry::Occupied(mut entry) => match entry.get() {
                        // Replace a hash node with a fully revealed branch node.
                        SparseNode::Hash(hash) => {
                            entry.insert(SparseNode::Branch {
                                state_mask: branch.state_mask,
                                // Memoize the hash of a previously blinded node in a new branch
                                // node.
                                hash: Some(*hash),
                                store_in_db_trie: Some(
                                    masks.hash_mask.is_some_and(|mask| !mask.is_empty()) ||
                                        masks.tree_mask.is_some_and(|mask| !mask.is_empty()),
                                ),
                            });
                        }
                        // Branch node already exists, or an extension node was placed where a
                        // branch node was before.
                        SparseNode::Branch { .. } | SparseNode::Extension { .. } => {}
                        // All other node types can't be handled.
                        node @ (SparseNode::Empty | SparseNode::Leaf { .. }) => {
                            return Err(SparseTrieErrorKind::Reveal {
                                path: *entry.key(),
                                node: Box::new(node.clone()),
                            }
                            .into())
                        }
                    },
                    Entry::Vacant(entry) => {
                        entry.insert(SparseNode::new_branch(branch.state_mask));
                    }
                }
            }
            TrieNode::Extension(ext) => match self.nodes.entry(path) {
                Entry::Occupied(mut entry) => match entry.get() {
                    // Replace a hash node with a revealed extension node.
                    SparseNode::Hash(hash) => {
                        let mut child_path = *entry.key();
                        child_path.extend(&ext.key);
                        entry.insert(SparseNode::Extension {
                            key: ext.key,
                            // Memoize the hash of a previously blinded node in a new extension
                            // node.
                            hash: Some(*hash),
                            store_in_db_trie: None,
                        });
                        if Self::is_child_same_level(&path, &child_path) {
                            self.reveal_node_or_hash(child_path, &ext.child)?;
                        }
                    }
                    // Extension node already exists, or an extension node was placed where a branch
                    // node was before.
                    SparseNode::Extension { .. } | SparseNode::Branch { .. } => {}
                    // All other node types can't be handled.
                    node @ (SparseNode::Empty | SparseNode::Leaf { .. }) => {
                        return Err(SparseTrieErrorKind::Reveal {
                            path: *entry.key(),
                            node: Box::new(node.clone()),
                        }
                        .into())
                    }
                },
                Entry::Vacant(entry) => {
                    let mut child_path = *entry.key();
                    child_path.extend(&ext.key);
                    entry.insert(SparseNode::new_ext(ext.key));
                    if Self::is_child_same_level(&path, &child_path) {
                        self.reveal_node_or_hash(child_path, &ext.child)?;
                    }
                }
            },
            TrieNode::Leaf(leaf) => match self.nodes.entry(path) {
                Entry::Occupied(mut entry) => match entry.get() {
                    // Replace a hash node with a revealed leaf node and store leaf node value.
                    SparseNode::Hash(hash) => {
                        let mut full = *entry.key();
                        full.extend(&leaf.key);
                        self.inner.values.insert(full, leaf.value.clone());
                        entry.insert(SparseNode::Leaf {
                            key: leaf.key,
                            // Memoize the hash of a previously blinded node in a new leaf
                            // node.
                            hash: Some(*hash),
                        });
                    }
                    // Leaf node already exists.
                    SparseNode::Leaf { .. } => {}
                    // All other node types can't be handled.
                    node @ (SparseNode::Empty |
                    SparseNode::Extension { .. } |
                    SparseNode::Branch { .. }) => {
                        return Err(SparseTrieErrorKind::Reveal {
                            path: *entry.key(),
                            node: Box::new(node.clone()),
                        }
                        .into())
                    }
                },
                Entry::Vacant(entry) => {
                    let mut full = *entry.key();
                    full.extend(&leaf.key);
                    entry.insert(SparseNode::new_leaf(leaf.key));
                    self.inner.values.insert(full, leaf.value.clone());
                }
            },
        }

        Ok(())
    }

    /// Reveals either a node or its hash placeholder based on the provided child data.
    ///
    /// When traversing the trie, we often encounter references to child nodes that
    /// are either directly embedded or represented by their hash. This method
    /// handles both cases:
    ///
    /// 1. If the child data represents a hash (32+1=33 bytes), store it as a hash node
    /// 2. Otherwise, decode the data as a [`TrieNode`] and recursively reveal it using
    ///    `reveal_node`
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if successful, or an error if the node cannot be revealed.
    ///
    /// # Error Handling
    ///
    /// Will error if there's a conflict between a new hash node and an existing one
    /// at the same path
    fn reveal_node_or_hash(&mut self, path: Nibbles, child: &[u8]) -> SparseTrieResult<()> {
        if child.len() == B256::len_bytes() + 1 {
            let hash = B256::from_slice(&child[1..]);
            match self.nodes.entry(path) {
                Entry::Occupied(entry) => match entry.get() {
                    // Hash node with a different hash can't be handled.
                    SparseNode::Hash(previous_hash) if previous_hash != &hash => {
                        return Err(SparseTrieErrorKind::Reveal {
                            path: *entry.key(),
                            node: Box::new(SparseNode::Hash(hash)),
                        }
                        .into())
                    }
                    _ => {}
                },
                Entry::Vacant(entry) => {
                    entry.insert(SparseNode::Hash(hash));
                }
            }
            return Ok(())
        }

        self.reveal_node(path, &TrieNode::decode(&mut &child[..])?, TrieMasks::none())
    }

    /// Recalculates and updates the RLP hashes for the changed nodes in this subtrie.
    ///
    /// The function starts from the subtrie root, traverses down to leaves, and then calculates
    /// the hashes from leaves back up to the root. It uses a stack from [`SparseSubtrieBuffers`] to
    /// track the traversal and accumulate RLP encodings.
    ///
    /// # Parameters
    ///
    /// - `prefix_set`: The set of trie paths whose nodes have changed.
    ///
    /// # Returns
    ///
    /// A tuple containing the root node of the updated subtrie and an optional set of updates.
    /// Updates are [`Some`] if [`Self::with_updates`] was set to `true`.
    ///
    /// # Panics
    ///
    /// If the node at the root path does not exist.
    #[instrument(level = "trace", target = "engine::tree", skip_all, fields(root = ?self.path), ret)]
    pub fn update_hashes(&mut self, prefix_set: &mut PrefixSet) -> RlpNode {
        trace!(target: "trie::parallel_sparse", "Updating subtrie hashes");

        debug_assert!(prefix_set.iter().all(|path| path.starts_with(&self.path)));

        debug_assert!(self.inner.buffers.path_stack.is_empty());
        self.inner
            .buffers
            .path_stack
            .push(RlpNodePathStackItem { path: self.path, is_in_prefix_set: None });

        while let Some(stack_item) = self.inner.buffers.path_stack.pop() {
            let path = stack_item.path;
            let node = self
                .nodes
                .get_mut(&path)
                .unwrap_or_else(|| panic!("node at path {path:?} does not exist"));

            self.inner.rlp_node(prefix_set, stack_item, node);
        }

        debug_assert_eq!(self.inner.buffers.rlp_node_stack.len(), 1);
        self.inner.buffers.rlp_node_stack.pop().unwrap().rlp_node
    }

    /// Consumes and returns the currently accumulated trie updates.
    ///
    /// This is useful when you want to apply the updates to an external database,
    /// and then start tracking a new set of updates.
    fn take_updates(&mut self) -> SparseTrieUpdates {
        self.inner.updates.take().unwrap_or_default()
    }
}

/// Helper type for [`SparseSubtrie`] to mutably access only a subset of fields from the original
/// struct.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct SparseSubtrieInner {
    /// When a branch is set, the corresponding child subtree is stored in the database.
    branch_node_tree_masks: HashMap<Nibbles, TrieMask>,
    /// When a bit is set, the corresponding child is stored as a hash in the database.
    branch_node_hash_masks: HashMap<Nibbles, TrieMask>,
    /// Map from leaf key paths to their values.
    /// All values are stored here instead of directly in leaf nodes.
    values: HashMap<Nibbles, Vec<u8>>,
    /// Optional tracking of trie updates for later use.
    updates: Option<SparseTrieUpdates>,
    /// Reusable buffers for [`SparseSubtrie::update_hashes`].
    buffers: SparseSubtrieBuffers,
}

impl SparseSubtrieInner {
    /// Computes the RLP encoding and its hash for a single (trie node)[`SparseNode`].
    ///
    /// # Deferred Processing
    ///
    /// When an extension or a branch node depends on child nodes that haven't been computed yet,
    /// the function pushes the current node back onto the path stack along with its children,
    /// then returns early. This allows the iterative algorithm to process children first before
    /// retrying the parent.
    ///
    /// # Parameters
    ///
    /// - `prefix_set`: Set of prefixes (key paths) that have been marked as updated
    /// - `stack_item`: The stack item to process
    /// - `node`: The sparse node to process (will be mutated to update hash)
    ///
    /// # Side Effects
    ///
    /// - Updates the node's hash field after computing RLP
    /// - Pushes nodes to [`SparseSubtrieBuffers::path_stack`] to manage traversal
    /// - Updates the (trie updates)[`SparseTrieUpdates`] accumulator when tracking changes, if
    ///   [`Some`]
    /// - May push items onto the path stack for deferred processing
    ///
    /// # Exit condition
    ///
    /// Once all nodes have been processed and all RLPs and hashes calculated, pushes the root node
    /// onto the [`SparseSubtrieBuffers::rlp_node_stack`] and exits.
    fn rlp_node(
        &mut self,
        prefix_set: &mut PrefixSet,
        mut stack_item: RlpNodePathStackItem,
        node: &mut SparseNode,
    ) {
        let path = stack_item.path;
        trace!(
            target: "trie::parallel_sparse",
            ?path,
            ?node,
            "Calculating node RLP"
        );

        // Check if the path is in the prefix set.
        // First, check the cached value. If it's `None`, then check the prefix set, and update
        // the cached value.
        let mut prefix_set_contains = |path: &Nibbles| {
            *stack_item.is_in_prefix_set.get_or_insert_with(|| prefix_set.contains(path))
        };

        let (rlp_node, node_type) = match node {
            SparseNode::Empty => (RlpNode::word_rlp(&EMPTY_ROOT_HASH), SparseNodeType::Empty),
            SparseNode::Hash(hash) => {
                // Return pre-computed hash of a blinded node immediately
                (RlpNode::word_rlp(hash), SparseNodeType::Hash)
            }
            SparseNode::Leaf { key, hash } => {
                let mut path = path;
                path.extend(key);
                if let Some(hash) = hash.filter(|_| !prefix_set_contains(&path)) {
                    // If the node hash is already computed, and the node path is not in
                    // the prefix set, return the pre-computed hash
                    (RlpNode::word_rlp(&hash), SparseNodeType::Leaf)
                } else {
                    // Encode the leaf node and update its hash
                    let value = self.values.get(&path).unwrap();
                    self.buffers.rlp_buf.clear();
                    let rlp_node = LeafNodeRef { key, value }.rlp(&mut self.buffers.rlp_buf);
                    *hash = rlp_node.as_hash();
                    (rlp_node, SparseNodeType::Leaf)
                }
            }
            SparseNode::Extension { key, hash, store_in_db_trie } => {
                let mut child_path = path;
                child_path.extend(key);
                if let Some((hash, store_in_db_trie)) =
                    hash.zip(*store_in_db_trie).filter(|_| !prefix_set_contains(&path))
                {
                    // If the node hash is already computed, and the node path is not in
                    // the prefix set, return the pre-computed hash
                    (
                        RlpNode::word_rlp(&hash),
                        SparseNodeType::Extension { store_in_db_trie: Some(store_in_db_trie) },
                    )
                } else if self.buffers.rlp_node_stack.last().is_some_and(|e| e.path == child_path) {
                    // Top of the stack has the child node, we can encode the extension node and
                    // update its hash
                    let RlpNodeStackItem { path: _, rlp_node: child, node_type: child_node_type } =
                        self.buffers.rlp_node_stack.pop().unwrap();
                    self.buffers.rlp_buf.clear();
                    let rlp_node =
                        ExtensionNodeRef::new(key, &child).rlp(&mut self.buffers.rlp_buf);
                    *hash = rlp_node.as_hash();

                    let store_in_db_trie_value = child_node_type.store_in_db_trie();

                    trace!(
                        target: "trie::parallel_sparse",
                        ?path,
                        ?child_path,
                        ?child_node_type,
                        "Extension node"
                    );

                    *store_in_db_trie = store_in_db_trie_value;

                    (
                        rlp_node,
                        SparseNodeType::Extension {
                            // Inherit the `store_in_db_trie` flag from the child node, which is
                            // always the branch node
                            store_in_db_trie: store_in_db_trie_value,
                        },
                    )
                } else {
                    // Need to defer processing until child is computed, on the next
                    // invocation update the node's hash.
                    self.buffers.path_stack.extend([
                        RlpNodePathStackItem {
                            path,
                            is_in_prefix_set: Some(prefix_set_contains(&path)),
                        },
                        RlpNodePathStackItem { path: child_path, is_in_prefix_set: None },
                    ]);
                    return
                }
            }
            SparseNode::Branch { state_mask, hash, store_in_db_trie } => {
                if let Some((hash, store_in_db_trie)) =
                    hash.zip(*store_in_db_trie).filter(|_| !prefix_set_contains(&path))
                {
                    // If the node hash is already computed, and the node path is not in
                    // the prefix set, return the pre-computed hash
                    self.buffers.rlp_node_stack.push(RlpNodeStackItem {
                        path,
                        rlp_node: RlpNode::word_rlp(&hash),
                        node_type: SparseNodeType::Branch {
                            store_in_db_trie: Some(store_in_db_trie),
                        },
                    });
                    return
                }

                let retain_updates = self.updates.is_some() && prefix_set_contains(&path);

                self.buffers.branch_child_buf.clear();
                // Walk children in a reverse order from `f` to `0`, so we pop the `0` first
                // from the stack and keep walking in the sorted order.
                for bit in CHILD_INDEX_RANGE.rev() {
                    if state_mask.is_bit_set(bit) {
                        let mut child = path;
                        child.push_unchecked(bit);
                        self.buffers.branch_child_buf.push(child);
                    }
                }

                self.buffers
                    .branch_value_stack_buf
                    .resize(self.buffers.branch_child_buf.len(), Default::default());
                let mut added_children = false;

                let mut tree_mask = TrieMask::default();
                let mut hash_mask = TrieMask::default();
                let mut hashes = Vec::new();
                for (i, child_path) in self.buffers.branch_child_buf.iter().enumerate() {
                    if self.buffers.rlp_node_stack.last().is_some_and(|e| &e.path == child_path) {
                        let RlpNodeStackItem {
                            path: _,
                            rlp_node: child,
                            node_type: child_node_type,
                        } = self.buffers.rlp_node_stack.pop().unwrap();

                        // Update the masks only if we need to retain trie updates
                        if retain_updates {
                            // SAFETY: it's a child, so it's never empty
                            let last_child_nibble = child_path.last().unwrap();

                            // Determine whether we need to set trie mask bit.
                            let should_set_tree_mask_bit = if let Some(store_in_db_trie) =
                                child_node_type.store_in_db_trie()
                            {
                                // A branch or an extension node explicitly set the
                                // `store_in_db_trie` flag
                                store_in_db_trie
                            } else {
                                // A blinded node has the tree mask bit set
                                child_node_type.is_hash() &&
                                    self.branch_node_tree_masks
                                        .get(&path)
                                        .is_some_and(|mask| mask.is_bit_set(last_child_nibble))
                            };
                            if should_set_tree_mask_bit {
                                tree_mask.set_bit(last_child_nibble);
                            }

                            // Set the hash mask. If a child node is a revealed branch node OR
                            // is a blinded node that has its hash mask bit set according to the
                            // database, set the hash mask bit and save the hash.
                            let hash = child.as_hash().filter(|_| {
                                child_node_type.is_branch() ||
                                    (child_node_type.is_hash() &&
                                        self.branch_node_hash_masks.get(&path).is_some_and(
                                            |mask| mask.is_bit_set(last_child_nibble),
                                        ))
                            });
                            if let Some(hash) = hash {
                                hash_mask.set_bit(last_child_nibble);
                                hashes.push(hash);
                            }
                        }

                        // Insert children in the resulting buffer in a normal order,
                        // because initially we iterated in reverse.
                        // SAFETY: i < len and len is never 0
                        let original_idx = self.buffers.branch_child_buf.len() - i - 1;
                        self.buffers.branch_value_stack_buf[original_idx] = child;
                        added_children = true;
                    } else {
                        // Need to defer processing until children are computed, on the next
                        // invocation update the node's hash.
                        debug_assert!(!added_children);
                        self.buffers.path_stack.push(RlpNodePathStackItem {
                            path,
                            is_in_prefix_set: Some(prefix_set_contains(&path)),
                        });
                        self.buffers.path_stack.extend(
                            self.buffers
                                .branch_child_buf
                                .drain(..)
                                .map(|path| RlpNodePathStackItem { path, is_in_prefix_set: None }),
                        );
                        return
                    }
                }

                trace!(
                    target: "trie::parallel_sparse",
                    ?path,
                    ?tree_mask,
                    ?hash_mask,
                    "Branch node masks"
                );

                // Top of the stack has all children node, we can encode the branch node and
                // update its hash
                self.buffers.rlp_buf.clear();
                let branch_node_ref =
                    BranchNodeRef::new(&self.buffers.branch_value_stack_buf, *state_mask);
                let rlp_node = branch_node_ref.rlp(&mut self.buffers.rlp_buf);
                *hash = rlp_node.as_hash();

                // Save a branch node update only if it's not a root node, and we need to
                // persist updates.
                let store_in_db_trie_value = if let Some(updates) =
                    self.updates.as_mut().filter(|_| retain_updates && !path.is_empty())
                {
                    let store_in_db_trie = !tree_mask.is_empty() || !hash_mask.is_empty();
                    if store_in_db_trie {
                        // Store in DB trie if there are either any children that are stored in
                        // the DB trie, or any children represent hashed values
                        hashes.reverse();
                        let branch_node = BranchNodeCompact::new(
                            *state_mask,
                            tree_mask,
                            hash_mask,
                            hashes,
                            hash.filter(|_| path.is_empty()),
                        );
                        updates.updated_nodes.insert(path, branch_node);
                    } else if self
                        .branch_node_tree_masks
                        .get(&path)
                        .is_some_and(|mask| !mask.is_empty()) ||
                        self.branch_node_hash_masks
                            .get(&path)
                            .is_some_and(|mask| !mask.is_empty())
                    {
                        // If new tree and hash masks are empty, but previously they weren't, we
                        // need to remove the node update and add the node itself to the list of
                        // removed nodes.
                        updates.updated_nodes.remove(&path);
                        updates.removed_nodes.insert(path);
                    } else if self
                        .branch_node_hash_masks
                        .get(&path)
                        .is_none_or(|mask| mask.is_empty()) &&
                        self.branch_node_hash_masks.get(&path).is_none_or(|mask| mask.is_empty())
                    {
                        // If new tree and hash masks are empty, and they were previously empty
                        // as well, we need to remove the node update.
                        updates.updated_nodes.remove(&path);
                    }

                    store_in_db_trie
                } else {
                    false
                };
                *store_in_db_trie = Some(store_in_db_trie_value);

                (
                    rlp_node,
                    SparseNodeType::Branch { store_in_db_trie: Some(store_in_db_trie_value) },
                )
            }
        };

        self.buffers.rlp_node_stack.push(RlpNodeStackItem { path, rlp_node, node_type });
        trace!(
            target: "trie::parallel_sparse",
            ?path,
            ?node_type,
            "Added node to RLP node stack"
        );
    }
}

/// Sparse Subtrie Type.
///
/// Used to determine the type of subtrie a certain path belongs to:
/// - Paths in the range `0x..=0xf` belong to the upper subtrie.
/// - Paths in the range `0x00..` belong to one of the lower subtries. The index of the lower
///   subtrie is determined by the first [`UPPER_TRIE_MAX_DEPTH`] nibbles of the path.
///
/// There can be at most [`NUM_LOWER_SUBTRIES`] lower subtries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SparseSubtrieType {
    /// Upper subtrie with paths in the range `0x..=0xf`
    Upper,
    /// Lower subtrie with paths in the range `0x00..`. Includes the index of the subtrie,
    /// according to the path prefix.
    Lower(usize),
}

impl SparseSubtrieType {
    /// Returns true if a node at a path of the given length would be placed in the upper subtrie.
    ///
    /// Nodes with paths shorter than [`UPPER_TRIE_MAX_DEPTH`] nibbles belong to the upper subtrie,
    /// while longer paths belong to the lower subtries.
    pub const fn path_len_is_upper(len: usize) -> bool {
        len < UPPER_TRIE_MAX_DEPTH
    }

    /// Returns the type of subtrie based on the given path.
    pub fn from_path(path: &Nibbles) -> Self {
        if Self::path_len_is_upper(path.len()) {
            Self::Upper
        } else {
            Self::Lower(path_subtrie_index_unchecked(path))
        }
    }

    /// Returns the index of the lower subtrie, if it exists.
    pub const fn lower_index(&self) -> Option<usize> {
        match self {
            Self::Upper => None,
            Self::Lower(index) => Some(*index),
        }
    }
}

/// Collection of reusable buffers for calculating subtrie hashes.
///
/// These buffers reduce allocations when computing RLP representations during trie updates.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SparseSubtrieBuffers {
    /// Stack of RLP node paths
    path_stack: Vec<RlpNodePathStackItem>,
    /// Stack of RLP nodes
    rlp_node_stack: Vec<RlpNodeStackItem>,
    /// Reusable branch child path
    branch_child_buf: SmallVec<[Nibbles; 16]>,
    /// Reusable branch value stack
    branch_value_stack_buf: SmallVec<[RlpNode; 16]>,
    /// Reusable RLP buffer
    rlp_buf: Vec<u8>,
}

/// RLP node path stack item.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RlpNodePathStackItem {
    /// Path to the node.
    pub path: Nibbles,
    /// Whether the path is in the prefix set. If [`None`], then unknown yet.
    pub is_in_prefix_set: Option<bool>,
}

/// Changed subtrie.
#[derive(Debug)]
struct ChangedSubtrie {
    /// Lower subtrie index in the range [0, [`NUM_LOWER_SUBTRIES`]).
    index: usize,
    /// Changed subtrie
    subtrie: Box<SparseSubtrie>,
    /// Prefix set of keys that belong to the subtrie.
    #[allow(unused)]
    prefix_set: PrefixSet,
}

/// Convert first [`UPPER_TRIE_MAX_DEPTH`] nibbles of the path into a lower subtrie index in the
/// range [0, [`NUM_LOWER_SUBTRIES`]).
///
/// # Panics
///
/// If the path is shorter than [`UPPER_TRIE_MAX_DEPTH`] nibbles.
fn path_subtrie_index_unchecked(path: &Nibbles) -> usize {
    debug_assert_eq!(UPPER_TRIE_MAX_DEPTH, 2);
    path.get_byte_unchecked(0) as usize
}

#[cfg(test)]
mod tests {
    use super::{
        path_subtrie_index_unchecked, ParallelSparseTrie, SparseSubtrie, SparseSubtrieType,
    };
    use crate::trie::ChangedSubtrie;
    use alloy_primitives::{
        map::{foldhash::fast::RandomState, B256Set, DefaultHashBuilder, HashMap},
        B256,
    };
    use alloy_rlp::{Decodable, Encodable};
    use alloy_trie::{BranchNodeCompact, Nibbles};
    use assert_matches::assert_matches;
    use itertools::Itertools;
    use reth_execution_errors::SparseTrieError;
    use reth_primitives_traits::Account;
    use reth_trie::{
        hashed_cursor::{noop::NoopHashedAccountCursor, HashedPostStateAccountCursor},
        node_iter::{TrieElement, TrieNodeIter},
        trie_cursor::{noop::NoopAccountTrieCursor, TrieCursor},
        walker::TrieWalker,
    };
    use reth_trie_common::{
        prefix_set::PrefixSetMut,
        proof::{ProofNodes, ProofRetainer},
        updates::TrieUpdates,
        BranchNode, ExtensionNode, HashBuilder, HashedPostState, LeafNode, RlpNode, TrieMask,
        TrieNode, EMPTY_ROOT_HASH,
    };
    use reth_trie_sparse::{
        blinded::{BlindedProvider, RevealedNode},
        SparseNode, TrieMasks,
    };

    /// Mock blinded provider for testing that allows pre-setting nodes at specific paths.
    ///
    /// This provider can be used in tests to simulate blinded nodes that need to be revealed
    /// during trie operations, particularly when collapsing branch nodes during leaf removal.
    #[derive(Debug, Clone)]
    struct MockBlindedProvider {
        /// Mapping from path to revealed node data
        nodes: HashMap<Nibbles, RevealedNode, DefaultHashBuilder>,
    }

    impl MockBlindedProvider {
        /// Creates a new empty mock provider
        fn new() -> Self {
            Self { nodes: HashMap::with_hasher(RandomState::default()) }
        }

        /// Adds a revealed node at the specified path
        fn add_revealed_node(&mut self, path: Nibbles, node: RevealedNode) {
            self.nodes.insert(path, node);
        }
    }

    impl BlindedProvider for MockBlindedProvider {
        fn blinded_node(&self, path: &Nibbles) -> Result<Option<RevealedNode>, SparseTrieError> {
            Ok(self.nodes.get(path).cloned())
        }
    }

    fn create_account(nonce: u64) -> Account {
        Account { nonce, ..Default::default() }
    }

    fn encode_account_value(nonce: u64) -> Vec<u8> {
        let account = Account { nonce, ..Default::default() };
        let trie_account = account.into_trie_account(EMPTY_ROOT_HASH);
        let mut buf = Vec::new();
        trie_account.encode(&mut buf);
        buf
    }

    fn create_leaf_node(key: impl AsRef<[u8]>, value_nonce: u64) -> TrieNode {
        TrieNode::Leaf(LeafNode::new(Nibbles::from_nibbles(key), encode_account_value(value_nonce)))
    }

    fn create_extension_node(key: impl AsRef<[u8]>, child_hash: B256) -> TrieNode {
        TrieNode::Extension(ExtensionNode::new(
            Nibbles::from_nibbles(key),
            RlpNode::word_rlp(&child_hash),
        ))
    }

    fn create_branch_node_with_children(
        children_indices: &[u8],
        child_hashes: impl IntoIterator<Item = RlpNode>,
    ) -> TrieNode {
        let mut stack = Vec::new();
        let mut state_mask = TrieMask::default();

        for (&idx, hash) in children_indices.iter().zip(child_hashes.into_iter()) {
            state_mask.set_bit(idx);
            stack.push(hash);
        }

        TrieNode::Branch(BranchNode::new(stack, state_mask))
    }

    /// Calculate the state root by feeding the provided state to the hash builder and retaining the
    /// proofs for the provided targets.
    ///
    /// Returns the state root and the retained proof nodes.
    fn run_hash_builder(
        state: impl IntoIterator<Item = (Nibbles, Account)> + Clone,
        trie_cursor: impl TrieCursor,
        destroyed_accounts: B256Set,
        proof_targets: impl IntoIterator<Item = Nibbles>,
    ) -> (B256, TrieUpdates, ProofNodes, HashMap<Nibbles, TrieMask>, HashMap<Nibbles, TrieMask>)
    {
        let mut account_rlp = Vec::new();

        let mut hash_builder = HashBuilder::default()
            .with_updates(true)
            .with_proof_retainer(ProofRetainer::from_iter(proof_targets));

        let mut prefix_set = PrefixSetMut::default();
        prefix_set.extend_keys(state.clone().into_iter().map(|(nibbles, _)| nibbles));
        prefix_set.extend_keys(destroyed_accounts.iter().map(Nibbles::unpack));
        let walker =
            TrieWalker::state_trie(trie_cursor, prefix_set.freeze()).with_deletions_retained(true);
        let hashed_post_state = HashedPostState::default()
            .with_accounts(state.into_iter().map(|(nibbles, account)| {
                (nibbles.pack().into_inner().unwrap().into(), Some(account))
            }))
            .into_sorted();
        let mut node_iter = TrieNodeIter::state_trie(
            walker,
            HashedPostStateAccountCursor::new(
                NoopHashedAccountCursor::default(),
                hashed_post_state.accounts(),
            ),
        );

        while let Some(node) = node_iter.try_next().unwrap() {
            match node {
                TrieElement::Branch(branch) => {
                    hash_builder.add_branch(branch.key, branch.value, branch.children_are_in_trie);
                }
                TrieElement::Leaf(key, account) => {
                    let account = account.into_trie_account(EMPTY_ROOT_HASH);
                    account.encode(&mut account_rlp);

                    hash_builder.add_leaf(Nibbles::unpack(key), &account_rlp);
                    account_rlp.clear();
                }
            }
        }
        let root = hash_builder.root();
        let proof_nodes = hash_builder.take_proof_nodes();
        let branch_node_hash_masks = hash_builder
            .updated_branch_nodes
            .clone()
            .unwrap_or_default()
            .iter()
            .map(|(path, node)| (*path, node.hash_mask))
            .collect();
        let branch_node_tree_masks = hash_builder
            .updated_branch_nodes
            .clone()
            .unwrap_or_default()
            .iter()
            .map(|(path, node)| (*path, node.tree_mask))
            .collect();

        let mut trie_updates = TrieUpdates::default();
        let removed_keys = node_iter.walker.take_removed_keys();
        trie_updates.finalize(hash_builder, removed_keys, destroyed_accounts);

        (root, trie_updates, proof_nodes, branch_node_hash_masks, branch_node_tree_masks)
    }

    /// Returns a `ParallelSparseTrie` pre-loaded with the given nodes, as well as leaf values
    /// inferred from any provided leaf nodes.
    fn new_test_trie<Nodes>(nodes: Nodes) -> ParallelSparseTrie
    where
        Nodes: Iterator<Item = (Nibbles, SparseNode)>,
    {
        let mut trie = ParallelSparseTrie::default().with_updates(true);

        for (path, node) in nodes {
            let subtrie = trie.subtrie_for_path(&path);
            if let SparseNode::Leaf { key, .. } = &node {
                let mut full_key = path;
                full_key.extend(key);
                subtrie.inner.values.insert(full_key, "LEAF VALUE".into());
            }
            subtrie.nodes.insert(path, node);
        }
        trie
    }

    /// Assert that the parallel sparse trie nodes and the proof nodes from the hash builder are
    /// equal.
    #[allow(unused)]
    fn assert_eq_parallel_sparse_trie_proof_nodes(
        sparse_trie: &ParallelSparseTrie,
        proof_nodes: ProofNodes,
    ) {
        let proof_nodes = proof_nodes
            .into_nodes_sorted()
            .into_iter()
            .map(|(path, node)| (path, TrieNode::decode(&mut node.as_ref()).unwrap()));

        let lower_sparse_nodes = sparse_trie
            .lower_subtries
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|subtrie| subtrie.nodes.iter());

        let upper_sparse_nodes = sparse_trie.upper_subtrie.nodes.iter();

        let all_sparse_nodes =
            lower_sparse_nodes.chain(upper_sparse_nodes).sorted_by_key(|(path, _)| *path);

        for ((proof_node_path, proof_node), (sparse_node_path, sparse_node)) in
            proof_nodes.zip(all_sparse_nodes)
        {
            assert_eq!(&proof_node_path, sparse_node_path);

            let equals = match (&proof_node, &sparse_node) {
                // Both nodes are empty
                (TrieNode::EmptyRoot, SparseNode::Empty) => true,
                // Both nodes are branches and have the same state mask
                (
                    TrieNode::Branch(BranchNode { state_mask: proof_state_mask, .. }),
                    SparseNode::Branch { state_mask: sparse_state_mask, .. },
                ) => proof_state_mask == sparse_state_mask,
                // Both nodes are extensions and have the same key
                (
                    TrieNode::Extension(ExtensionNode { key: proof_key, .. }),
                    SparseNode::Extension { key: sparse_key, .. },
                ) |
                // Both nodes are leaves and have the same key
                (
                    TrieNode::Leaf(LeafNode { key: proof_key, .. }),
                    SparseNode::Leaf { key: sparse_key, .. },
                ) => proof_key == sparse_key,
                // Empty and hash nodes are specific to the sparse trie, skip them
                (_, SparseNode::Empty | SparseNode::Hash(_)) => continue,
                _ => false,
            };
            assert!(
                equals,
                "path: {proof_node_path:?}\nproof node: {proof_node:?}\nsparse node: {sparse_node:?}"
            );
        }
    }

    #[test]
    fn test_get_changed_subtries_empty() {
        let mut trie = ParallelSparseTrie::default();
        let mut prefix_set = PrefixSetMut::from([Nibbles::default()]).freeze();

        let (subtries, unchanged_prefix_set) = trie.take_changed_lower_subtries(&mut prefix_set);
        assert!(subtries.is_empty());
        assert_eq!(unchanged_prefix_set, PrefixSetMut::from(prefix_set.iter().copied()));
    }

    #[test]
    fn test_get_changed_subtries() {
        // Create a trie with three subtries
        let mut trie = ParallelSparseTrie::default();
        let subtrie_1 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x0, 0x0])));
        let subtrie_1_index = path_subtrie_index_unchecked(&subtrie_1.path);
        let subtrie_2 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x1, 0x0])));
        let subtrie_2_index = path_subtrie_index_unchecked(&subtrie_2.path);
        let subtrie_3 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x3, 0x0])));
        let subtrie_3_index = path_subtrie_index_unchecked(&subtrie_3.path);

        // Add subtries at specific positions
        trie.lower_subtries[subtrie_1_index] = Some(subtrie_1.clone());
        trie.lower_subtries[subtrie_2_index] = Some(subtrie_2.clone());
        trie.lower_subtries[subtrie_3_index] = Some(subtrie_3);

        let unchanged_prefix_set = PrefixSetMut::from([
            Nibbles::from_nibbles([0x0]),
            Nibbles::from_nibbles([0x2, 0x0, 0x0]),
        ]);
        // Create a prefix set with the keys that match only the second subtrie
        let mut prefix_set = PrefixSetMut::from([
            // Match second subtrie
            Nibbles::from_nibbles([0x1, 0x0, 0x0]),
            Nibbles::from_nibbles([0x1, 0x0, 0x1, 0x0]),
        ]);
        prefix_set.extend(unchanged_prefix_set);
        let mut prefix_set = prefix_set.freeze();

        // Second subtrie should be removed and returned
        let (subtries, unchanged_prefix_set) = trie.take_changed_lower_subtries(&mut prefix_set);
        assert_eq!(
            subtries
                .into_iter()
                .map(|ChangedSubtrie { index, subtrie, prefix_set }| {
                    (index, subtrie, prefix_set.iter().copied().collect::<Vec<_>>())
                })
                .collect::<Vec<_>>(),
            vec![(
                subtrie_2_index,
                subtrie_2,
                vec![
                    Nibbles::from_nibbles([0x1, 0x0, 0x0]),
                    Nibbles::from_nibbles([0x1, 0x0, 0x1, 0x0])
                ]
            )]
        );
        assert_eq!(unchanged_prefix_set, unchanged_prefix_set);
        assert!(trie.lower_subtries[subtrie_2_index].is_none());

        // First subtrie should remain unchanged
        assert_eq!(trie.lower_subtries[subtrie_1_index], Some(subtrie_1));
    }

    #[test]
    fn test_get_changed_subtries_all() {
        // Create a trie with three subtries
        let mut trie = ParallelSparseTrie::default();
        let subtrie_1 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x0, 0x0])));
        let subtrie_1_index = path_subtrie_index_unchecked(&subtrie_1.path);
        let subtrie_2 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x1, 0x0])));
        let subtrie_2_index = path_subtrie_index_unchecked(&subtrie_2.path);
        let subtrie_3 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x3, 0x0])));
        let subtrie_3_index = path_subtrie_index_unchecked(&subtrie_3.path);

        // Add subtries at specific positions
        trie.lower_subtries[subtrie_1_index] = Some(subtrie_1.clone());
        trie.lower_subtries[subtrie_2_index] = Some(subtrie_2.clone());
        trie.lower_subtries[subtrie_3_index] = Some(subtrie_3.clone());

        // Create a prefix set that matches any key
        let mut prefix_set = PrefixSetMut::all().freeze();

        // All subtries should be removed and returned
        let (subtries, unchanged_prefix_set) = trie.take_changed_lower_subtries(&mut prefix_set);
        assert_eq!(
            subtries
                .into_iter()
                .map(|ChangedSubtrie { index, subtrie, prefix_set }| {
                    (index, subtrie, prefix_set.all())
                })
                .collect::<Vec<_>>(),
            vec![
                (subtrie_1_index, subtrie_1, true),
                (subtrie_2_index, subtrie_2, true),
                (subtrie_3_index, subtrie_3, true)
            ]
        );
        assert_eq!(unchanged_prefix_set, PrefixSetMut::all());

        assert!(trie.lower_subtries.iter().all(Option::is_none));
    }

    #[test]
    fn test_sparse_subtrie_type() {
        assert_eq!(SparseSubtrieType::from_path(&Nibbles::new()), SparseSubtrieType::Upper);
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([0])),
            SparseSubtrieType::Upper
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([15])),
            SparseSubtrieType::Upper
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([0, 0])),
            SparseSubtrieType::Lower(0)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([0, 0, 0])),
            SparseSubtrieType::Lower(0)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([0, 1])),
            SparseSubtrieType::Lower(1)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([0, 1, 0])),
            SparseSubtrieType::Lower(1)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([0, 15])),
            SparseSubtrieType::Lower(15)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([15, 0])),
            SparseSubtrieType::Lower(240)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([15, 1])),
            SparseSubtrieType::Lower(241)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([15, 15])),
            SparseSubtrieType::Lower(255)
        );
        assert_eq!(
            SparseSubtrieType::from_path(&Nibbles::from_nibbles([15, 15, 15])),
            SparseSubtrieType::Lower(255)
        );
    }

    #[test]
    fn test_reveal_node_leaves() {
        let mut trie = ParallelSparseTrie::default();

        // Reveal leaf in the upper trie
        {
            let path = Nibbles::from_nibbles([0x1]);
            let node = create_leaf_node([0x2, 0x3], 42);
            let masks = TrieMasks::none();

            trie.reveal_node(path, node, masks).unwrap();

            assert_matches!(
                trie.upper_subtrie.nodes.get(&path),
                Some(SparseNode::Leaf { key, hash: None })
                if key == &Nibbles::from_nibbles([0x2, 0x3])
            );

            let full_path = Nibbles::from_nibbles([0x1, 0x2, 0x3]);
            assert_eq!(
                trie.upper_subtrie.inner.values.get(&full_path),
                Some(&encode_account_value(42))
            );
        }

        // Reveal leaf in a lower trie
        {
            let path = Nibbles::from_nibbles([0x1, 0x2]);
            let node = create_leaf_node([0x3, 0x4], 42);
            let masks = TrieMasks::none();

            trie.reveal_node(path, node, masks).unwrap();

            // Check that the lower subtrie was created
            let idx = path_subtrie_index_unchecked(&path);
            assert!(trie.lower_subtries[idx].is_some());

            let lower_subtrie = trie.lower_subtries[idx].as_ref().unwrap();
            assert_matches!(
                lower_subtrie.nodes.get(&path),
                Some(SparseNode::Leaf { key, hash: None })
                if key == &Nibbles::from_nibbles([0x3, 0x4])
            );
        }
    }

    #[test]
    fn test_reveal_node_extension_all_upper() {
        let mut trie = ParallelSparseTrie::default();
        let path = Nibbles::new();
        let child_hash = B256::repeat_byte(0xab);
        let node = create_extension_node([0x1], child_hash);
        let masks = TrieMasks::none();

        trie.reveal_node(path, node, masks).unwrap();

        assert_matches!(
            trie.upper_subtrie.nodes.get(&path),
            Some(SparseNode::Extension { key, hash: None, .. })
            if key == &Nibbles::from_nibbles([0x1])
        );

        // Child path should be in upper trie
        let child_path = Nibbles::from_nibbles([0x1]);
        assert_eq!(trie.upper_subtrie.nodes.get(&child_path), Some(&SparseNode::Hash(child_hash)));
    }

    #[test]
    fn test_reveal_node_extension_cross_level() {
        let mut trie = ParallelSparseTrie::default();
        let path = Nibbles::new();
        let child_hash = B256::repeat_byte(0xcd);
        let node = create_extension_node([0x1, 0x2, 0x3], child_hash);
        let masks = TrieMasks::none();

        trie.reveal_node(path, node, masks).unwrap();

        // Extension node should be in upper trie
        assert_matches!(
            trie.upper_subtrie.nodes.get(&path),
            Some(SparseNode::Extension { key, hash: None, .. })
            if key == &Nibbles::from_nibbles([0x1, 0x2, 0x3])
        );

        // Child path (0x1, 0x2, 0x3) should be in lower trie
        let child_path = Nibbles::from_nibbles([0x1, 0x2, 0x3]);
        let idx = path_subtrie_index_unchecked(&child_path);
        assert!(trie.lower_subtries[idx].is_some());

        let lower_subtrie = trie.lower_subtries[idx].as_ref().unwrap();
        assert_eq!(lower_subtrie.nodes.get(&child_path), Some(&SparseNode::Hash(child_hash)));
    }

    #[test]
    fn test_reveal_node_extension_cross_level_boundary() {
        let mut trie = ParallelSparseTrie::default();
        let path = Nibbles::from_nibbles([0x1]);
        let child_hash = B256::repeat_byte(0xcd);
        let node = create_extension_node([0x2], child_hash);
        let masks = TrieMasks::none();

        trie.reveal_node(path, node, masks).unwrap();

        // Extension node should be in upper trie
        assert_matches!(
            trie.upper_subtrie.nodes.get(&path),
            Some(SparseNode::Extension { key, hash: None, .. })
            if key == &Nibbles::from_nibbles([0x2])
        );

        // Child path (0x1, 0x2) should be in lower trie
        let child_path = Nibbles::from_nibbles([0x1, 0x2]);
        let idx = path_subtrie_index_unchecked(&child_path);
        assert!(trie.lower_subtries[idx].is_some());

        let lower_subtrie = trie.lower_subtries[idx].as_ref().unwrap();
        assert_eq!(lower_subtrie.nodes.get(&child_path), Some(&SparseNode::Hash(child_hash)));
    }

    #[test]
    fn test_reveal_node_branch_all_upper() {
        let mut trie = ParallelSparseTrie::default();
        let path = Nibbles::new();
        let child_hashes = [
            RlpNode::word_rlp(&B256::repeat_byte(0x11)),
            RlpNode::word_rlp(&B256::repeat_byte(0x22)),
        ];
        let node = create_branch_node_with_children(&[0x0, 0x5], child_hashes.clone());
        let masks = TrieMasks::none();

        trie.reveal_node(path, node, masks).unwrap();

        // Branch node should be in upper trie
        assert_matches!(
            trie.upper_subtrie.nodes.get(&path),
            Some(SparseNode::Branch { state_mask, hash: None, .. })
            if *state_mask == 0b0000000000100001.into()
        );

        // Children should be in upper trie (paths of length 2)
        let child_path_0 = Nibbles::from_nibbles([0x0]);
        let child_path_5 = Nibbles::from_nibbles([0x5]);
        assert_eq!(
            trie.upper_subtrie.nodes.get(&child_path_0),
            Some(&SparseNode::Hash(child_hashes[0].as_hash().unwrap()))
        );
        assert_eq!(
            trie.upper_subtrie.nodes.get(&child_path_5),
            Some(&SparseNode::Hash(child_hashes[1].as_hash().unwrap()))
        );
    }

    #[test]
    fn test_reveal_node_branch_cross_level() {
        let mut trie = ParallelSparseTrie::default();
        let path = Nibbles::from_nibbles([0x1]); // Exactly 1 nibbles - boundary case
        let child_hashes = [
            RlpNode::word_rlp(&B256::repeat_byte(0x33)),
            RlpNode::word_rlp(&B256::repeat_byte(0x44)),
            RlpNode::word_rlp(&B256::repeat_byte(0x55)),
        ];
        let node = create_branch_node_with_children(&[0x0, 0x7, 0xf], child_hashes.clone());
        let masks = TrieMasks::none();

        trie.reveal_node(path, node, masks).unwrap();

        // Branch node should be in upper trie
        assert_matches!(
            trie.upper_subtrie.nodes.get(&path),
            Some(SparseNode::Branch { state_mask, hash: None, .. })
            if *state_mask == 0b1000000010000001.into()
        );

        // All children should be in lower tries since they have paths of length 3
        let child_paths = [
            Nibbles::from_nibbles([0x1, 0x0]),
            Nibbles::from_nibbles([0x1, 0x7]),
            Nibbles::from_nibbles([0x1, 0xf]),
        ];

        for (i, child_path) in child_paths.iter().enumerate() {
            let idx = path_subtrie_index_unchecked(child_path);
            let lower_subtrie = trie.lower_subtries[idx].as_ref().unwrap();
            assert_eq!(
                lower_subtrie.nodes.get(child_path),
                Some(&SparseNode::Hash(child_hashes[i].as_hash().unwrap())),
            );
        }
    }

    #[test]
    fn test_update_subtrie_hashes() {
        // Create a trie with three subtries
        let mut trie = ParallelSparseTrie::default();
        let mut subtrie_1 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x0, 0x0])));
        let subtrie_1_index = path_subtrie_index_unchecked(&subtrie_1.path);
        let mut subtrie_2 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x1, 0x0])));
        let subtrie_2_index = path_subtrie_index_unchecked(&subtrie_2.path);
        let mut subtrie_3 = Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x3, 0x0])));
        let subtrie_3_index = path_subtrie_index_unchecked(&subtrie_3.path);

        // Reveal dummy leaf nodes that form an incorrect trie structure but enough to test the
        // method
        let leaf_1_full_path = Nibbles::from_nibbles([0; 64]);
        let leaf_1_path = leaf_1_full_path.slice(..2);
        let leaf_1_key = leaf_1_full_path.slice(2..);
        let leaf_2_full_path = Nibbles::from_nibbles([vec![1, 0], vec![0; 62]].concat());
        let leaf_2_path = leaf_2_full_path.slice(..2);
        let leaf_2_key = leaf_2_full_path.slice(2..);
        let leaf_3_full_path = Nibbles::from_nibbles([vec![3, 0], vec![0; 62]].concat());
        let leaf_3_path = leaf_3_full_path.slice(..2);
        let leaf_3_key = leaf_3_full_path.slice(2..);
        let leaf_1 = create_leaf_node(leaf_1_key.to_vec(), 1);
        let leaf_2 = create_leaf_node(leaf_2_key.to_vec(), 2);
        let leaf_3 = create_leaf_node(leaf_3_key.to_vec(), 3);
        subtrie_1.reveal_node(leaf_1_path, &leaf_1, TrieMasks::none()).unwrap();
        subtrie_2.reveal_node(leaf_2_path, &leaf_2, TrieMasks::none()).unwrap();
        subtrie_3.reveal_node(leaf_3_path, &leaf_3, TrieMasks::none()).unwrap();

        // Add subtries at specific positions
        trie.lower_subtries[subtrie_1_index] = Some(subtrie_1);
        trie.lower_subtries[subtrie_2_index] = Some(subtrie_2);
        trie.lower_subtries[subtrie_3_index] = Some(subtrie_3);

        let unchanged_prefix_set = PrefixSetMut::from([
            Nibbles::from_nibbles([0x0]),
            Nibbles::from_nibbles([0x2, 0x0, 0x0]),
        ]);
        // Create a prefix set with the keys that match only the second subtrie
        let mut prefix_set = PrefixSetMut::from([
            // Match second subtrie
            Nibbles::from_nibbles([0x1, 0x0, 0x0]),
            Nibbles::from_nibbles([0x1, 0x0, 0x1, 0x0]),
        ]);
        prefix_set.extend(unchanged_prefix_set.clone());
        trie.prefix_set = prefix_set;

        // Update subtrie hashes
        trie.update_lower_subtrie_hashes();

        // Check that the prefix set was updated
        assert_eq!(trie.prefix_set, unchanged_prefix_set);
        // Check that subtries were returned back to the array
        assert!(trie.lower_subtries[subtrie_1_index].is_some());
        assert!(trie.lower_subtries[subtrie_2_index].is_some());
        assert!(trie.lower_subtries[subtrie_3_index].is_some());
    }

    #[test]
    fn test_subtrie_update_hashes() {
        let mut subtrie =
            Box::new(SparseSubtrie::new(Nibbles::from_nibbles([0x0, 0x0])).with_updates(true));

        // Create leaf nodes with paths 0x0...0, 0x00001...0, 0x0010...0
        let leaf_1_full_path = Nibbles::from_nibbles([0; 64]);
        let leaf_1_path = leaf_1_full_path.slice(..5);
        let leaf_1_key = leaf_1_full_path.slice(5..);
        let leaf_2_full_path = Nibbles::from_nibbles([vec![0, 0, 0, 0, 1], vec![0; 59]].concat());
        let leaf_2_path = leaf_2_full_path.slice(..5);
        let leaf_2_key = leaf_2_full_path.slice(5..);
        let leaf_3_full_path = Nibbles::from_nibbles([vec![0, 0, 1], vec![0; 61]].concat());
        let leaf_3_path = leaf_3_full_path.slice(..3);
        let leaf_3_key = leaf_3_full_path.slice(3..);

        let account_1 = create_account(1);
        let account_2 = create_account(2);
        let account_3 = create_account(3);
        let leaf_1 = create_leaf_node(leaf_1_key.to_vec(), account_1.nonce);
        let leaf_2 = create_leaf_node(leaf_2_key.to_vec(), account_2.nonce);
        let leaf_3 = create_leaf_node(leaf_3_key.to_vec(), account_3.nonce);

        // Create bottom branch node
        let branch_1_path = Nibbles::from_nibbles([0, 0, 0, 0]);
        let branch_1 = create_branch_node_with_children(
            &[0, 1],
            vec![
                RlpNode::from_rlp(&alloy_rlp::encode(&leaf_1)),
                RlpNode::from_rlp(&alloy_rlp::encode(&leaf_2)),
            ],
        );

        // Create an extension node
        let extension_path = Nibbles::from_nibbles([0, 0, 0]);
        let extension_key = Nibbles::from_nibbles([0]);
        let extension = create_extension_node(
            extension_key.to_vec(),
            RlpNode::from_rlp(&alloy_rlp::encode(&branch_1)).as_hash().unwrap(),
        );

        // Create top branch node
        let branch_2_path = Nibbles::from_nibbles([0, 0]);
        let branch_2 = create_branch_node_with_children(
            &[0, 1],
            vec![
                RlpNode::from_rlp(&alloy_rlp::encode(&extension)),
                RlpNode::from_rlp(&alloy_rlp::encode(&leaf_3)),
            ],
        );

        // Reveal nodes
        subtrie.reveal_node(branch_2_path, &branch_2, TrieMasks::none()).unwrap();
        subtrie.reveal_node(leaf_1_path, &leaf_1, TrieMasks::none()).unwrap();
        subtrie.reveal_node(extension_path, &extension, TrieMasks::none()).unwrap();
        subtrie.reveal_node(branch_1_path, &branch_1, TrieMasks::none()).unwrap();
        subtrie.reveal_node(leaf_2_path, &leaf_2, TrieMasks::none()).unwrap();
        subtrie.reveal_node(leaf_3_path, &leaf_3, TrieMasks::none()).unwrap();

        // Run hash builder for two leaf nodes
        let (_, _, proof_nodes, _, _) = run_hash_builder(
            [
                (leaf_1_full_path, account_1),
                (leaf_2_full_path, account_2),
                (leaf_3_full_path, account_3),
            ],
            NoopAccountTrieCursor::default(),
            Default::default(),
            [
                branch_1_path,
                extension_path,
                branch_2_path,
                leaf_1_full_path,
                leaf_2_full_path,
                leaf_3_full_path,
            ],
        );

        // Update hashes for the subtrie
        subtrie.update_hashes(
            &mut PrefixSetMut::from([leaf_1_full_path, leaf_2_full_path, leaf_3_full_path])
                .freeze(),
        );

        // Compare hashes between hash builder and subtrie

        let hash_builder_branch_1_hash =
            RlpNode::from_rlp(proof_nodes.get(&branch_1_path).unwrap().as_ref()).as_hash().unwrap();
        let subtrie_branch_1_hash = subtrie.nodes.get(&branch_1_path).unwrap().hash().unwrap();
        assert_eq!(hash_builder_branch_1_hash, subtrie_branch_1_hash);

        let hash_builder_extension_hash =
            RlpNode::from_rlp(proof_nodes.get(&extension_path).unwrap().as_ref())
                .as_hash()
                .unwrap();
        let subtrie_extension_hash = subtrie.nodes.get(&extension_path).unwrap().hash().unwrap();
        assert_eq!(hash_builder_extension_hash, subtrie_extension_hash);

        let hash_builder_branch_2_hash =
            RlpNode::from_rlp(proof_nodes.get(&branch_2_path).unwrap().as_ref()).as_hash().unwrap();
        let subtrie_branch_2_hash = subtrie.nodes.get(&branch_2_path).unwrap().hash().unwrap();
        assert_eq!(hash_builder_branch_2_hash, subtrie_branch_2_hash);

        let subtrie_leaf_1_hash = subtrie.nodes.get(&leaf_1_path).unwrap().hash().unwrap();
        let hash_builder_leaf_1_hash =
            RlpNode::from_rlp(proof_nodes.get(&leaf_1_path).unwrap().as_ref()).as_hash().unwrap();
        assert_eq!(hash_builder_leaf_1_hash, subtrie_leaf_1_hash);

        let hash_builder_leaf_2_hash =
            RlpNode::from_rlp(proof_nodes.get(&leaf_2_path).unwrap().as_ref()).as_hash().unwrap();
        let subtrie_leaf_2_hash = subtrie.nodes.get(&leaf_2_path).unwrap().hash().unwrap();
        assert_eq!(hash_builder_leaf_2_hash, subtrie_leaf_2_hash);

        let hash_builder_leaf_3_hash =
            RlpNode::from_rlp(proof_nodes.get(&leaf_3_path).unwrap().as_ref()).as_hash().unwrap();
        let subtrie_leaf_3_hash = subtrie.nodes.get(&leaf_3_path).unwrap().hash().unwrap();
        assert_eq!(hash_builder_leaf_3_hash, subtrie_leaf_3_hash);
    }

    #[test]
    fn test_remove_leaf_branch_becomes_extension() {
        //
        // 0x:      Extension (Key = 5)
        // 0x5:     └── Branch (Mask = 1001)
        // 0x50:        ├── 0 -> Extension (Key = 23)
        // 0x5023:      │        └── Branch (Mask = 0101)
        // 0x50231:     │            ├── 1 -> Leaf
        // 0x50233:     │            └── 3 -> Leaf
        // 0x53:        └── 3 -> Leaf (Key = 7)
        //
        // After removing 0x53, extension+branch+extension become a single extension
        //
        let mut trie = new_test_trie(
            [
                (Nibbles::default(), SparseNode::new_ext(Nibbles::from_nibbles([0x5]))),
                (Nibbles::from_nibbles([0x5]), SparseNode::new_branch(TrieMask::new(0b1001))),
                (
                    Nibbles::from_nibbles([0x5, 0x0]),
                    SparseNode::new_ext(Nibbles::from_nibbles([0x2, 0x3])),
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3]),
                    SparseNode::new_branch(TrieMask::new(0b0101)),
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x1]),
                    SparseNode::new_leaf(Nibbles::new()),
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3, 0x3]),
                    SparseNode::new_leaf(Nibbles::new()),
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x3]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x7])),
                ),
            ]
            .into_iter(),
        );

        let provider = MockBlindedProvider::new();

        // Remove the leaf with a full path of 0x537
        let leaf_full_path = Nibbles::from_nibbles([0x5, 0x3, 0x7]);
        trie.remove_leaf(&leaf_full_path, provider).unwrap();

        let upper_subtrie = &trie.upper_subtrie;
        let lower_subtrie_50 = trie.lower_subtries[0x50].as_ref().unwrap();
        let lower_subtrie_53 = trie.lower_subtries[0x53].as_ref().unwrap();

        // Check that the leaf value was removed from the appropriate `SparseSubtrie`.
        assert_matches!(lower_subtrie_53.inner.values.get(&leaf_full_path), None);

        // Check that the leaf node was removed, and that its parent/grandparent were modified
        // appropriately.
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::from_nibbles([])),
            Some(SparseNode::Extension{ key, ..})
            if key == &Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3])
        );
        assert_matches!(upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x5])), None);
        assert_matches!(lower_subtrie_50.nodes.get(&Nibbles::from_nibbles([0x5, 0x0])), None);
        assert_matches!(
            lower_subtrie_50.nodes.get(&Nibbles::from_nibbles([0x5, 0x0, 0x2, 0x3])),
            Some(SparseNode::Branch{ state_mask, .. })
            if *state_mask == 0b0101.into()
        );
        assert_matches!(lower_subtrie_53.nodes.get(&Nibbles::from_nibbles([0x5, 0x3])), None);
    }

    #[test]
    fn test_remove_leaf_branch_becomes_leaf() {
        //
        // 0x:      Branch (Mask = 0011)
        // 0x0:     ├── 0 -> Leaf (Key = 12)
        // 0x1:     └── 1 -> Leaf (Key = 34)
        //
        // After removing 0x012, branch becomes a leaf
        //
        let mut trie = new_test_trie(
            [
                (Nibbles::default(), SparseNode::new_branch(TrieMask::new(0b0011))),
                (
                    Nibbles::from_nibbles([0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x1, 0x2])),
                ),
                (
                    Nibbles::from_nibbles([0x1]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x3, 0x4])),
                ),
            ]
            .into_iter(),
        );

        // Add the branch node to updated_nodes to simulate it being modified earlier
        if let Some(updates) = trie.updates.as_mut() {
            updates
                .updated_nodes
                .insert(Nibbles::default(), BranchNodeCompact::new(0b11, 0, 0, vec![], None));
        }

        let provider = MockBlindedProvider::new();

        // Remove the leaf with a full path of 0x012
        let leaf_full_path = Nibbles::from_nibbles([0x0, 0x1, 0x2]);
        trie.remove_leaf(&leaf_full_path, provider).unwrap();

        let upper_subtrie = &trie.upper_subtrie;

        // Check that the leaf's value was removed
        assert_matches!(upper_subtrie.inner.values.get(&leaf_full_path), None);

        // Check that the branch node collapsed into a leaf node with the remaining child's key
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::default()),
            Some(SparseNode::Leaf{ key, ..})
            if key == &Nibbles::from_nibbles([0x1, 0x3, 0x4])
        );

        // Check that the remaining child node was removed
        assert_matches!(upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x1])), None);
        // Check that the removed child node was also removed
        assert_matches!(upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x0])), None);

        // Check that updates were tracked correctly when branch collapsed
        let updates = trie.updates.as_ref().unwrap();

        // The branch at root should be marked as removed since it collapsed
        assert!(updates.removed_nodes.contains(&Nibbles::default()));

        // The branch should no longer be in updated_nodes
        assert!(!updates.updated_nodes.contains_key(&Nibbles::default()));
    }

    #[test]
    fn test_remove_leaf_extension_becomes_leaf() {
        //
        // 0x:      Extension (Key = 5)
        // 0x5:     └── Branch (Mask = 0011)
        // 0x50:        ├── 0 -> Leaf (Key = 12)
        // 0x51:        └── 1 -> Leaf (Key = 34)
        //
        // After removing 0x5012, extension+branch becomes a leaf
        //
        let mut trie = new_test_trie(
            [
                (Nibbles::default(), SparseNode::new_ext(Nibbles::from_nibbles([0x5]))),
                (Nibbles::from_nibbles([0x5]), SparseNode::new_branch(TrieMask::new(0b0011))),
                (
                    Nibbles::from_nibbles([0x5, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x1, 0x2])),
                ),
                (
                    Nibbles::from_nibbles([0x5, 0x1]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x3, 0x4])),
                ),
            ]
            .into_iter(),
        );

        let provider = MockBlindedProvider::new();

        // Remove the leaf with a full path of 0x5012
        let leaf_full_path = Nibbles::from_nibbles([0x5, 0x0, 0x1, 0x2]);
        trie.remove_leaf(&leaf_full_path, provider).unwrap();

        let upper_subtrie = &trie.upper_subtrie;
        let lower_subtrie_50 = trie.lower_subtries[0x50].as_ref().unwrap();
        let lower_subtrie_51 = trie.lower_subtries[0x51].as_ref().unwrap();

        // Check that the full key was removed
        assert_matches!(lower_subtrie_50.inner.values.get(&leaf_full_path), None);

        // Check that the other leaf's value was moved to the upper trie
        let other_leaf_full_value = Nibbles::from_nibbles([0x5, 0x1, 0x3, 0x4]);
        assert_matches!(lower_subtrie_51.inner.values.get(&other_leaf_full_value), None);
        assert_matches!(upper_subtrie.inner.values.get(&other_leaf_full_value), Some(_));

        // Check that the extension node collapsed into a leaf node
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::default()),
            Some(SparseNode::Leaf{ key, ..})
            if key == &Nibbles::from_nibbles([0x5, 0x1, 0x3, 0x4])
        );

        // Check that intermediate nodes were removed
        assert_matches!(upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x5])), None);
        assert_matches!(lower_subtrie_50.nodes.get(&Nibbles::from_nibbles([0x5, 0x0])), None);
        assert_matches!(lower_subtrie_51.nodes.get(&Nibbles::from_nibbles([0x5, 0x1])), None);
    }

    #[test]
    fn test_remove_leaf_branch_on_branch() {
        //
        // 0x:      Branch (Mask = 0101)
        // 0x0:     ├── 0 -> Leaf (Key = 12)
        // 0x2:     └── 2 -> Branch (Mask = 0011)
        // 0x20:        ├── 0 -> Leaf (Key = 34)
        // 0x21:        └── 1 -> Leaf (Key = 56)
        //
        // After removing 0x2034, the inner branch becomes a leaf
        //
        let mut trie = new_test_trie(
            [
                (Nibbles::default(), SparseNode::new_branch(TrieMask::new(0b0101))),
                (
                    Nibbles::from_nibbles([0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x1, 0x2])),
                ),
                (Nibbles::from_nibbles([0x2]), SparseNode::new_branch(TrieMask::new(0b0011))),
                (
                    Nibbles::from_nibbles([0x2, 0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x3, 0x4])),
                ),
                (
                    Nibbles::from_nibbles([0x2, 0x1]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x5, 0x6])),
                ),
            ]
            .into_iter(),
        );

        let provider = MockBlindedProvider::new();

        // Remove the leaf with a full path of 0x2034
        let leaf_full_path = Nibbles::from_nibbles([0x2, 0x0, 0x3, 0x4]);
        trie.remove_leaf(&leaf_full_path, provider).unwrap();

        let upper_subtrie = &trie.upper_subtrie;
        let lower_subtrie_20 = trie.lower_subtries[0x20].as_ref().unwrap();
        let lower_subtrie_21 = trie.lower_subtries[0x21].as_ref().unwrap();

        // Check that the leaf's value was removed
        assert_matches!(lower_subtrie_20.inner.values.get(&leaf_full_path), None);

        // Check that the other leaf's value was moved to the upper trie
        let other_leaf_full_value = Nibbles::from_nibbles([0x2, 0x1, 0x5, 0x6]);
        assert_matches!(lower_subtrie_21.inner.values.get(&other_leaf_full_value), None);
        assert_matches!(upper_subtrie.inner.values.get(&other_leaf_full_value), Some(_));

        // Check that the root branch still exists unchanged
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::default()),
            Some(SparseNode::Branch{ state_mask, .. })
            if *state_mask == 0b0101.into()
        );

        // Check that the inner branch became an extension
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x2])),
            Some(SparseNode::Leaf{ key, ..})
            if key == &Nibbles::from_nibbles([0x1, 0x5, 0x6])
        );

        // Check that the branch's child nodes were removed
        assert_matches!(lower_subtrie_20.nodes.get(&Nibbles::from_nibbles([0x2, 0x0])), None);
        assert_matches!(lower_subtrie_21.nodes.get(&Nibbles::from_nibbles([0x2, 0x1])), None);
    }

    #[test]
    fn test_remove_leaf_remaining_child_needs_reveal() {
        //
        // 0x:      Branch (Mask = 0011)
        // 0x0:     ├── 0 -> Leaf (Key = 12)
        // 0x1:     └── 1 -> Hash (blinded leaf)
        //
        // After removing 0x012, the hash node needs to be revealed to collapse the branch
        //
        let mut trie = new_test_trie(
            [
                (Nibbles::default(), SparseNode::new_branch(TrieMask::new(0b0011))),
                (
                    Nibbles::from_nibbles([0x0]),
                    SparseNode::new_leaf(Nibbles::from_nibbles([0x1, 0x2])),
                ),
                (Nibbles::from_nibbles([0x1]), SparseNode::Hash(B256::repeat_byte(0xab))),
            ]
            .into_iter(),
        );

        // Create a mock provider that will reveal the blinded leaf
        let mut provider = MockBlindedProvider::new();
        let revealed_leaf = create_leaf_node([0x3, 0x4], 42);
        let mut encoded = Vec::new();
        revealed_leaf.encode(&mut encoded);
        provider.add_revealed_node(
            Nibbles::from_nibbles([0x1]),
            RevealedNode { node: encoded.into(), tree_mask: None, hash_mask: None },
        );

        // Remove the leaf with a full path of 0x012
        let leaf_full_path = Nibbles::from_nibbles([0x0, 0x1, 0x2]);
        trie.remove_leaf(&leaf_full_path, provider).unwrap();

        let upper_subtrie = &trie.upper_subtrie;

        // Check that the leaf value was removed
        assert_matches!(upper_subtrie.inner.values.get(&leaf_full_path), None);

        // Check that the branch node collapsed into a leaf node with the revealed child's key
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::default()),
            Some(SparseNode::Leaf{ key, ..})
            if key == &Nibbles::from_nibbles([0x1, 0x3, 0x4])
        );

        // Check that the remaining child node was removed (since it was merged)
        assert_matches!(upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x1])), None);
    }

    #[test]
    fn test_remove_leaf_root() {
        //
        // 0x:      Leaf (Key = 123)
        //
        // After removing 0x123, the trie becomes empty
        //
        let mut trie = new_test_trie(std::iter::once((
            Nibbles::default(),
            SparseNode::new_leaf(Nibbles::from_nibbles([0x1, 0x2, 0x3])),
        )));

        let provider = MockBlindedProvider::new();

        // Remove the leaf with a full key of 0x123
        let leaf_full_path = Nibbles::from_nibbles([0x1, 0x2, 0x3]);
        trie.remove_leaf(&leaf_full_path, provider).unwrap();

        let upper_subtrie = &trie.upper_subtrie;

        // Check that the leaf value was removed
        assert_matches!(upper_subtrie.inner.values.get(&leaf_full_path), None);

        // Check that the root node was changed to Empty
        assert_matches!(upper_subtrie.nodes.get(&Nibbles::default()), Some(SparseNode::Empty));
    }

    #[test]
    fn test_remove_leaf_unsets_hash_along_path() {
        //
        // Creates a trie structure:
        // 0x:      Branch (with hash set)
        // 0x0:     ├── Extension (with hash set)
        // 0x01:    │   └── Branch (with hash set)
        // 0x012:   │       ├── Leaf (Key = 34, with hash set)
        // 0x013:   │       ├── Leaf (Key = 56, with hash set)
        // 0x014:   │       └── Leaf (Key = 78, with hash set)
        // 0x1:     └── Leaf (Key = 78, with hash set)
        //
        // When removing leaf at 0x01234, all nodes along the path (root branch,
        // extension at 0x0, branch at 0x01) should have their hash field unset
        //

        let mut trie = new_test_trie(
            [
                (
                    Nibbles::default(),
                    SparseNode::Branch {
                        state_mask: TrieMask::new(0b0011),
                        hash: Some(B256::repeat_byte(0x10)),
                        store_in_db_trie: None,
                    },
                ),
                (
                    Nibbles::from_nibbles([0x0]),
                    SparseNode::Extension {
                        key: Nibbles::from_nibbles([0x1]),
                        hash: Some(B256::repeat_byte(0x20)),
                        store_in_db_trie: None,
                    },
                ),
                (
                    Nibbles::from_nibbles([0x0, 0x1]),
                    SparseNode::Branch {
                        state_mask: TrieMask::new(0b11100),
                        hash: Some(B256::repeat_byte(0x30)),
                        store_in_db_trie: None,
                    },
                ),
                (
                    Nibbles::from_nibbles([0x0, 0x1, 0x2]),
                    SparseNode::Leaf {
                        key: Nibbles::from_nibbles([0x3, 0x4]),
                        hash: Some(B256::repeat_byte(0x40)),
                    },
                ),
                (
                    Nibbles::from_nibbles([0x0, 0x1, 0x3]),
                    SparseNode::Leaf {
                        key: Nibbles::from_nibbles([0x5, 0x6]),
                        hash: Some(B256::repeat_byte(0x50)),
                    },
                ),
                (
                    Nibbles::from_nibbles([0x0, 0x1, 0x4]),
                    SparseNode::Leaf {
                        key: Nibbles::from_nibbles([0x6, 0x7]),
                        hash: Some(B256::repeat_byte(0x60)),
                    },
                ),
                (
                    Nibbles::from_nibbles([0x1]),
                    SparseNode::Leaf {
                        key: Nibbles::from_nibbles([0x7, 0x8]),
                        hash: Some(B256::repeat_byte(0x70)),
                    },
                ),
            ]
            .into_iter(),
        );

        let provider = MockBlindedProvider::new();

        // Remove the leaf at path 0x01234
        let leaf_full_path = Nibbles::from_nibbles([0x0, 0x1, 0x2, 0x3, 0x4]);
        trie.remove_leaf(&leaf_full_path, provider).unwrap();

        let upper_subtrie = &trie.upper_subtrie;
        let lower_subtrie_10 = trie.lower_subtries[0x01].as_ref().unwrap();

        // Verify that hash fields are unset for all nodes along the path to the removed leaf
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::default()),
            Some(SparseNode::Branch { hash: None, .. })
        );
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x0])),
            Some(SparseNode::Extension { hash: None, .. })
        );
        assert_matches!(
            lower_subtrie_10.nodes.get(&Nibbles::from_nibbles([0x0, 0x1])),
            Some(SparseNode::Branch { hash: None, .. })
        );

        // Verify that nodes not on the path still have their hashes
        assert_matches!(
            upper_subtrie.nodes.get(&Nibbles::from_nibbles([0x1])),
            Some(SparseNode::Leaf { hash: Some(_), .. })
        );
        assert_matches!(
            lower_subtrie_10.nodes.get(&Nibbles::from_nibbles([0x0, 0x1, 0x3])),
            Some(SparseNode::Leaf { hash: Some(_), .. })
        );
        assert_matches!(
            lower_subtrie_10.nodes.get(&Nibbles::from_nibbles([0x0, 0x1, 0x4])),
            Some(SparseNode::Leaf { hash: Some(_), .. })
        );
    }

    #[test]
    fn test_parallel_sparse_trie_root() {
        let mut trie = ParallelSparseTrie::default().with_updates(true);

        // Step 1: Create the trie structure
        // Extension node at 0x with key 0x2 (goes to upper subtrie)
        let extension_path = Nibbles::new();
        let extension_key = Nibbles::from_nibbles([0x2]);

        // Branch node at 0x2 with children 0 and 1 (goes to upper subtrie)
        let branch_path = Nibbles::from_nibbles([0x2]);

        // Leaf nodes at 0x20 and 0x21 (go to lower subtries)
        let leaf_1_path = Nibbles::from_nibbles([0x2, 0x0]);
        let leaf_1_key = Nibbles::from_nibbles(vec![0; 62]); // Remaining key
        let leaf_1_full_path = Nibbles::from_nibbles([vec![0x2, 0x0], vec![0; 62]].concat());

        let leaf_2_path = Nibbles::from_nibbles([0x2, 0x1]);
        let leaf_2_key = Nibbles::from_nibbles(vec![0; 62]); // Remaining key
        let leaf_2_full_path = Nibbles::from_nibbles([vec![0x2, 0x1], vec![0; 62]].concat());

        // Create accounts
        let account_1 = create_account(1);
        let account_2 = create_account(2);

        // Create leaf nodes
        let leaf_1 = create_leaf_node(leaf_1_key.to_vec(), account_1.nonce);
        let leaf_2 = create_leaf_node(leaf_2_key.to_vec(), account_2.nonce);

        // Create branch node with children at indices 0 and 1
        let branch = create_branch_node_with_children(
            &[0, 1],
            vec![
                RlpNode::from_rlp(&alloy_rlp::encode(&leaf_1)),
                RlpNode::from_rlp(&alloy_rlp::encode(&leaf_2)),
            ],
        );

        // Create extension node pointing to branch
        let extension = create_extension_node(
            extension_key.to_vec(),
            RlpNode::from_rlp(&alloy_rlp::encode(&branch)).as_hash().unwrap(),
        );

        // Step 2: Reveal nodes in the trie
        trie.reveal_node(extension_path, extension, TrieMasks::none()).unwrap();
        trie.reveal_node(branch_path, branch, TrieMasks::none()).unwrap();
        trie.reveal_node(leaf_1_path, leaf_1, TrieMasks::none()).unwrap();
        trie.reveal_node(leaf_2_path, leaf_2, TrieMasks::none()).unwrap();

        // Step 3: Reset hashes for all revealed nodes to test actual hash calculation
        // Reset upper subtrie node hashes
        trie.upper_subtrie.nodes.get_mut(&extension_path).unwrap().set_hash(None);
        trie.upper_subtrie.nodes.get_mut(&branch_path).unwrap().set_hash(None);

        // Reset lower subtrie node hashes
        let leaf_1_subtrie_idx = path_subtrie_index_unchecked(&leaf_1_path);
        let leaf_2_subtrie_idx = path_subtrie_index_unchecked(&leaf_2_path);

        trie.lower_subtries[leaf_1_subtrie_idx]
            .as_mut()
            .unwrap()
            .nodes
            .get_mut(&leaf_1_path)
            .unwrap()
            .set_hash(None);
        trie.lower_subtries[leaf_2_subtrie_idx]
            .as_mut()
            .unwrap()
            .nodes
            .get_mut(&leaf_2_path)
            .unwrap()
            .set_hash(None);

        // Step 4: Add changed leaf node paths to prefix set
        trie.prefix_set.insert(leaf_1_full_path);
        trie.prefix_set.insert(leaf_2_full_path);

        // Step 5: Calculate root using our implementation
        let root = trie.root();

        // Step 6: Calculate root using HashBuilder for comparison
        let (hash_builder_root, _, _proof_nodes, _, _) = run_hash_builder(
            [(leaf_1_full_path, account_1), (leaf_2_full_path, account_2)],
            NoopAccountTrieCursor::default(),
            Default::default(),
            [extension_path, branch_path, leaf_1_full_path, leaf_2_full_path],
        );

        // Step 7: Verify the roots match
        assert_eq!(root, hash_builder_root);

        // Verify hashes were computed
        let leaf_1_subtrie = trie.lower_subtries[leaf_1_subtrie_idx].as_ref().unwrap();
        let leaf_2_subtrie = trie.lower_subtries[leaf_2_subtrie_idx].as_ref().unwrap();
        assert!(trie.upper_subtrie.nodes.get(&extension_path).unwrap().hash().is_some());
        assert!(trie.upper_subtrie.nodes.get(&branch_path).unwrap().hash().is_some());
        assert!(leaf_1_subtrie.nodes.get(&leaf_1_path).unwrap().hash().is_some());
        assert!(leaf_2_subtrie.nodes.get(&leaf_2_path).unwrap().hash().is_some());
    }
}
