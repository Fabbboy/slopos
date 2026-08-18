//! Coverage for the doubly-linked intrusive list backing task ownership: O(1)
//! removal from any position, and a node that knows which list owns it, so
//! `dlist_unlink` works without naming a head — the property task retirement
//! depends on.

use std::pin::Pin;

use slopos_ostd::sync::{DLink, DLinked, IntrusiveDList, LinkError, dlist_unlink};

enum RoleA {}
enum RoleB {}

struct Node {
    id: u32,
    a: DLink<Node, RoleA>,
    b: DLink<Node, RoleB>,
}

impl Node {
    fn new(id: u32) -> Pin<Box<Self>> {
        Box::pin(Self {
            id,
            a: DLink::new(),
            b: DLink::new(),
        })
    }
}

// SAFETY: each role returns a distinct, stable field of `Node`.
unsafe impl DLinked<RoleA> for Node {
    fn dlink(&self) -> &DLink<Node, RoleA> {
        &self.a
    }
}

// SAFETY: see above.
unsafe impl DLinked<RoleB> for Node {
    fn dlink(&self) -> &DLink<Node, RoleB> {
        &self.b
    }
}

fn ptr(node: &Pin<Box<Node>>) -> std::ptr::NonNull<Node> {
    std::ptr::NonNull::from(&**node)
}

fn ids<Role>(list: &IntrusiveDList<Node, Role>) -> Vec<u32>
where
    Node: DLinked<Role>,
{
    // SAFETY: every yielded node is a live member pinned by the caller.
    list.iter().map(|n| unsafe { n.as_ref().id }).collect()
}

#[test]
fn push_back_preserves_order_and_counts() {
    let list = IntrusiveDList::<Node, RoleA>::new();
    let nodes: Vec<_> = (1..=4).map(Node::new).collect();
    assert!(list.is_empty());

    for n in &nodes {
        list.push_back(ptr(n)).expect("fresh node links");
    }
    assert_eq!(ids(&list), vec![1, 2, 3, 4]);
    assert_eq!(list.len(), 4);
    assert!(!list.is_empty());
}

#[test]
fn double_push_is_rejected() {
    let list = IntrusiveDList::<Node, RoleA>::new();
    let node = Node::new(1);
    list.push_back(ptr(&node)).expect("first push");
    assert_eq!(list.push_back(ptr(&node)), Err(LinkError::AlreadyLinked));
    assert_eq!(list.len(), 1, "a rejected push must not inflate the count");
}

#[test]
fn a_node_belongs_to_at_most_one_list_of_a_role() {
    let first = IntrusiveDList::<Node, RoleA>::new();
    let second = IntrusiveDList::<Node, RoleA>::new();
    let node = Node::new(1);

    first.push_back(ptr(&node)).expect("first push");
    assert_eq!(
        second.push_back(ptr(&node)),
        Err(LinkError::AlreadyLinked),
        "membership is global to the role, not per list"
    );
    assert_eq!(second.len(), 0);
}

#[test]
fn remove_from_the_middle_is_exact() {
    let list = IntrusiveDList::<Node, RoleA>::new();
    let nodes: Vec<_> = (1..=5).map(Node::new).collect();
    for n in &nodes {
        list.push_back(ptr(n)).expect("push");
    }

    list.remove(ptr(&nodes[2])).expect("middle removal");
    assert_eq!(ids(&list), vec![1, 2, 4, 5]);

    list.remove(ptr(&nodes[0])).expect("head removal");
    assert_eq!(ids(&list), vec![2, 4, 5]);

    list.remove(ptr(&nodes[4])).expect("tail removal");
    assert_eq!(ids(&list), vec![2, 4]);
    assert_eq!(list.len(), 2);
}

#[test]
fn remove_rejects_a_node_owned_by_another_list() {
    let mine = IntrusiveDList::<Node, RoleA>::new();
    let theirs = IntrusiveDList::<Node, RoleA>::new();
    let node = Node::new(1);
    theirs.push_back(ptr(&node)).expect("push");

    assert_eq!(mine.remove(ptr(&node)), Err(LinkError::NotPresent));
    assert_eq!(theirs.len(), 1, "the owning list is untouched");
}

#[test]
fn pop_front_drains_in_order_then_empties() {
    let list = IntrusiveDList::<Node, RoleA>::new();
    let nodes: Vec<_> = (1..=3).map(Node::new).collect();
    for n in &nodes {
        list.push_back(ptr(n)).expect("push");
    }

    let mut drained = Vec::new();
    while let Some(n) = list.pop_front() {
        // SAFETY: the node is pinned by `nodes` for the whole test.
        drained.push(unsafe { n.as_ref().id });
    }
    assert_eq!(drained, vec![1, 2, 3]);
    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
    assert!(list.peek_front().is_none());
}

#[test]
fn peek_front_does_not_detach() {
    let list = IntrusiveDList::<Node, RoleA>::new();
    let node = Node::new(7);
    list.push_back(ptr(&node)).expect("push");

    let peeked = list.peek_front().expect("head present");
    // SAFETY: pinned by `node`.
    assert_eq!(unsafe { peeked.as_ref().id }, 7);
    assert_eq!(list.len(), 1, "peek leaves membership intact");
}

#[test]
fn dlist_unlink_finds_the_owning_list() {
    let parent = IntrusiveDList::<Node, RoleA>::new();
    let root = IntrusiveDList::<Node, RoleA>::new();
    let child = Node::new(1);
    let orphan = Node::new(2);

    parent.push_back(ptr(&child)).expect("push");
    root.push_back(ptr(&orphan)).expect("push");

    assert!(dlist_unlink::<Node, RoleA>(ptr(&child)));
    assert_eq!(parent.len(), 0, "unlinked from the parent list");
    assert_eq!(root.len(), 1, "the other list is untouched");

    assert!(dlist_unlink::<Node, RoleA>(ptr(&orphan)));
    assert_eq!(root.len(), 0);
}

#[test]
fn dlist_unlink_of_an_unlinked_node_is_a_no_op() {
    let node = Node::new(1);
    assert!(!dlist_unlink::<Node, RoleA>(ptr(&node)));
    assert!(!node.a.is_linked());
}

/// The shape adoption and orphaning use.
#[test]
fn a_node_moves_between_lists_without_double_membership() {
    let root = IntrusiveDList::<Node, RoleA>::new();
    let parent = IntrusiveDList::<Node, RoleA>::new();
    let node = Node::new(1);

    root.push_back(ptr(&node)).expect("register into root");
    assert!(node.a.is_linked());

    assert!(dlist_unlink::<Node, RoleA>(ptr(&node)));
    assert!(!node.a.is_linked(), "unlinked between the two halves");
    parent.push_back(ptr(&node)).expect("adopt");

    assert_eq!(root.len(), 0);
    assert_eq!(parent.len(), 1);
    assert!(node.a.is_linked());
}

#[test]
fn roles_are_independent() {
    let a = IntrusiveDList::<Node, RoleA>::new();
    let b = IntrusiveDList::<Node, RoleB>::new();
    let node = Node::new(1);

    a.push_back(ptr(&node)).expect("role A");
    b.push_back(ptr(&node)).expect("role B is a separate slot");
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);

    assert!(dlist_unlink::<Node, RoleA>(ptr(&node)));
    assert_eq!(a.len(), 0);
    assert_eq!(b.len(), 1, "role B membership survives a role A unlink");
    assert!(node.b.is_linked());
    assert!(!node.a.is_linked());

    assert!(dlist_unlink::<Node, RoleB>(ptr(&node)));
}

#[test]
fn reset_clears_a_copied_slot() {
    let list = IntrusiveDList::<Node, RoleA>::new();
    let node = Node::new(1);
    list.push_back(ptr(&node)).expect("push");
    assert!(node.a.is_linked());

    // Models fork: the child's copied bytes claim a membership it does not have.
    node.a.reset();
    assert!(!node.a.is_linked());
    assert!(!dlist_unlink::<Node, RoleA>(ptr(&node)));

    // The list still names it, so drain rather than leave a dangling head.
    let _ = list.pop_front();
}
