//! Host-side tests for `slopos_ostd::sync::intrusive`.

use core::ptr::NonNull;

use slopos_ostd::sync::intrusive::{IntrusiveLinkedList, Link, LinkError, Linked};

struct Node {
    value: u32,
    link: Link<Node>,
}

impl Node {
    fn new(value: u32) -> Box<Self> {
        Box::new(Self {
            value,
            link: Link::new(),
        })
    }
}

// SAFETY: `Node` embeds `link` at a stable offset; tests never
// move a node while it is linked into a list.
unsafe impl Linked for Node {
    fn link(&self) -> &Link<Node> {
        &self.link
    }
}

fn nn(b: &Box<Node>) -> NonNull<Node> {
    // SAFETY: a Box's interior is non-null and outlives the test.
    unsafe { NonNull::new_unchecked(Box::as_ref(b) as *const _ as *mut Node) }
}

#[test]
fn empty_list_state() {
    let list: IntrusiveLinkedList<Node> = IntrusiveLinkedList::new();
    assert_eq!(list.len(), 0);
    assert!(list.is_empty());
    assert!(list.pop().is_none());
}

#[test]
fn push_one_then_pop() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(7);
    list.push(nn(&a)).unwrap();
    assert_eq!(list.len(), 1);
    assert!(!list.is_empty());
    let popped = list.pop().expect("non-empty");
    // SAFETY: popped element still owned by `a`.
    let v = unsafe { popped.as_ref().value };
    assert_eq!(v, 7);
    assert_eq!(list.len(), 0);
    assert!(list.is_empty());
}

#[test]
fn fifo_order_pop() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(1);
    let b = Node::new(2);
    let c = Node::new(3);
    list.push(nn(&a)).unwrap();
    list.push(nn(&b)).unwrap();
    list.push(nn(&c)).unwrap();
    assert_eq!(list.len(), 3);

    let v: Vec<u32> = std::iter::from_fn(|| list.pop())
        // SAFETY: nodes still owned by their Boxes.
        .map(|p| unsafe { p.as_ref().value })
        .collect();
    assert_eq!(v, [1, 2, 3]);
}

#[test]
fn len_tracks_pushes_and_pops() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(10);
    let b = Node::new(20);
    list.push(nn(&a)).unwrap();
    assert_eq!(list.len(), 1);
    list.push(nn(&b)).unwrap();
    assert_eq!(list.len(), 2);
    list.pop();
    assert_eq!(list.len(), 1);
    list.pop();
    assert_eq!(list.len(), 0);
}

#[test]
fn remove_head() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(1);
    let b = Node::new(2);
    let c = Node::new(3);
    list.push(nn(&a)).unwrap();
    list.push(nn(&b)).unwrap();
    list.push(nn(&c)).unwrap();
    list.remove(nn(&a)).unwrap();
    assert_eq!(list.len(), 2);
    let v: Vec<u32> = std::iter::from_fn(|| list.pop())
        .map(|p| unsafe { p.as_ref().value })
        .collect();
    assert_eq!(v, [2, 3]);
}

#[test]
fn remove_middle() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(1);
    let b = Node::new(2);
    let c = Node::new(3);
    list.push(nn(&a)).unwrap();
    list.push(nn(&b)).unwrap();
    list.push(nn(&c)).unwrap();
    list.remove(nn(&b)).unwrap();
    assert_eq!(list.len(), 2);
    let v: Vec<u32> = std::iter::from_fn(|| list.pop())
        .map(|p| unsafe { p.as_ref().value })
        .collect();
    assert_eq!(v, [1, 3]);
}

#[test]
fn remove_tail() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(1);
    let b = Node::new(2);
    let c = Node::new(3);
    list.push(nn(&a)).unwrap();
    list.push(nn(&b)).unwrap();
    list.push(nn(&c)).unwrap();
    list.remove(nn(&c)).unwrap();
    assert_eq!(list.len(), 2);
    // After removing tail, push another to verify tail pointer is correct.
    let d = Node::new(4);
    list.push(nn(&d)).unwrap();
    let v: Vec<u32> = std::iter::from_fn(|| list.pop())
        .map(|p| unsafe { p.as_ref().value })
        .collect();
    assert_eq!(v, [1, 2, 4]);
}

#[test]
fn remove_not_present_returns_err() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(1);
    let outsider = Node::new(99);
    list.push(nn(&a)).unwrap();
    assert_eq!(list.remove(nn(&outsider)), Err(LinkError::NotPresent));
    assert_eq!(list.len(), 1);
}

#[test]
fn double_push_rejected() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(1);
    let b = Node::new(2);
    list.push(nn(&a)).unwrap();
    list.push(nn(&b)).unwrap();
    // `a` is the head; its `link.next` points at `b`, so `has_next`
    // returns true and a re-push is rejected.
    assert_eq!(list.push(nn(&a)), Err(LinkError::AlreadyLinked));
    assert_eq!(list.len(), 2);
}

#[test]
fn iter_yields_all_elements_in_fifo_order() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(10);
    let b = Node::new(20);
    let c = Node::new(30);
    list.push(nn(&a)).unwrap();
    list.push(nn(&b)).unwrap();
    list.push(nn(&c)).unwrap();
    let v: Vec<u32> = list
        .iter()
        // SAFETY: nodes still owned by their Boxes.
        .map(|p| unsafe { p.as_ref().value })
        .collect();
    assert_eq!(v, [10, 20, 30]);
    // iter() does not consume; len unchanged.
    assert_eq!(list.len(), 3);
}

#[test]
fn pop_then_push_clears_link_so_node_can_be_reused() {
    let list = IntrusiveLinkedList::<Node>::new();
    let a = Node::new(1);
    let b = Node::new(2);
    list.push(nn(&a)).unwrap();
    list.push(nn(&b)).unwrap();
    // Pop a (head); its link should be cleared so a re-push succeeds.
    let popped = list.pop().unwrap();
    assert_eq!(unsafe { popped.as_ref().value }, 1);
    // Re-push: a is the popped node, whose link is now null.
    list.push(nn(&a)).unwrap();
    let v: Vec<u32> = std::iter::from_fn(|| list.pop())
        .map(|p| unsafe { p.as_ref().value })
        .collect();
    assert_eq!(v, [2, 1]);
}
