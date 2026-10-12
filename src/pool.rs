//! Intrusive node pool.
//!
//! Orders live in a slab and link into price levels through indices, so the
//! hot path never touches the allocator. `alloc` pops from a free list,
//! `free` pushes back. The slab only grows when the free list runs dry.

use crate::types::{Order, Side};

/// Null index marker for list links and the free list head.
pub const NULL: u32 = u32::MAX;

/// One resting order's worth of state, linked intrusively into its level.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub id: u64,
    pub side: Side,
    pub price: u64,
    pub qty: u64,
    pub seq: u64,
    pub next: u32,
    pub prev: u32,
}

impl Node {
    fn free(next_free: u32) -> Self {
        Node {
            id: 0,
            side: Side::Bid,
            price: 0,
            qty: 0,
            seq: 0,
            next: next_free,
            prev: NULL,
        }
    }
}

pub struct NodePool {
    nodes: Vec<Node>,
    free_head: u32,
}

impl NodePool {
    pub fn with_capacity(cap: usize) -> Self {
        let mut pool = NodePool {
            nodes: Vec::with_capacity(cap),
            free_head: NULL,
        };
        pool.grow(cap.max(64));
        pool
    }

    fn grow(&mut self, n: usize) {
        let base = self.nodes.len() as u32;
        for i in 0..n {
            self.nodes.push(Node::free(base + i as u32 + 1));
        }
        let last = (base + n as u32 - 1) as usize;
        self.nodes[last].next = self.free_head;
        self.free_head = base;
    }

    pub fn alloc(&mut self, order: &Order) -> u32 {
        if self.free_head == NULL {
            self.grow(self.nodes.len().max(64));
        }
        let idx = self.free_head;
        let node = &mut self.nodes[idx as usize];
        self.free_head = node.next;
        *node = Node {
            id: order.id,
            side: order.side,
            price: order.price,
            qty: order.qty,
            seq: order.seq,
            next: NULL,
            prev: NULL,
        };
        idx
    }

    pub fn free(&mut self, idx: u32) {
        let node = &mut self.nodes[idx as usize];
        node.next = self.free_head;
        node.prev = NULL;
        self.free_head = idx;
    }

    pub fn get(&self, idx: u32) -> &Node {
        &self.nodes[idx as usize]
    }

    pub fn get_mut(&mut self, idx: u32) -> &mut Node {
        &mut self.nodes[idx as usize]
    }

    /// Total slots ever created. Only used by tests.
    pub fn capacity(&self) -> usize {
        self.nodes.len()
    }

    /// Free slots right now. Only used by tests.
    pub fn free_count(&self) -> usize {
        let mut n = 0;
        let mut cur = self.free_head;
        while cur != NULL {
            n += 1;
            cur = self.nodes[cur as usize].next;
        }
        n
    }
}
