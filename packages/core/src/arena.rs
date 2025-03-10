use std::ptr::NonNull;

use crate::{
    nodes::RenderReturn, nodes::VNode, virtual_dom::VirtualDom, AttributeValue, DynamicNode,
    ScopeId,
};
use bumpalo::boxed::Box as BumpBox;

/// An Element's unique identifier.
///
/// `ElementId` is a `usize` that is unique across the entire VirtualDOM - but not unique across time. If a component is
/// unmounted, then the `ElementId` will be reused for a new component.
#[cfg_attr(feature = "serialize", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ElementId(pub usize);

pub(crate) struct ElementRef {
    // the pathway of the real element inside the template
    pub path: ElementPath,

    // The actual template
    pub template: Option<NonNull<VNode<'static>>>,
}

#[derive(Clone, Copy)]
pub enum ElementPath {
    Deep(&'static [u8]),
    Root(usize),
}

impl ElementRef {
    pub(crate) fn none() -> Self {
        Self {
            template: None,
            path: ElementPath::Root(0),
        }
    }
}

impl VirtualDom {
    pub(crate) fn next_element(&mut self, template: &VNode, path: &'static [u8]) -> ElementId {
        self.next_reference(template, ElementPath::Deep(path))
    }

    pub(crate) fn next_root(&mut self, template: &VNode, path: usize) -> ElementId {
        self.next_reference(template, ElementPath::Root(path))
    }

    pub(crate) fn next_null(&mut self) -> ElementId {
        let entry = self.elements.vacant_entry();
        let id = entry.key();

        entry.insert(ElementRef::none());
        ElementId(id)
    }

    fn next_reference(&mut self, template: &VNode, path: ElementPath) -> ElementId {
        let entry = self.elements.vacant_entry();
        let id = entry.key();

        entry.insert(ElementRef {
            // We know this is non-null because it comes from a reference
            template: Some(unsafe { NonNull::new_unchecked(template as *const _ as *mut _) }),
            path,
        });
        ElementId(id)
    }

    pub(crate) fn reclaim(&mut self, el: ElementId) {
        self.try_reclaim(el)
            .unwrap_or_else(|| panic!("cannot reclaim {:?}", el));
    }

    pub(crate) fn try_reclaim(&mut self, el: ElementId) -> Option<ElementRef> {
        if el.0 == 0 {
            panic!(
                "Cannot reclaim the root element - {:#?}",
                std::backtrace::Backtrace::force_capture()
            );
        }

        self.elements.try_remove(el.0)
    }

    pub(crate) fn update_template(&mut self, el: ElementId, node: &VNode) {
        let node: *const VNode = node as *const _;
        self.elements[el.0].template = unsafe { std::mem::transmute(node) };
    }

    // Drop a scope and all its children
    pub(crate) fn drop_scope(&mut self, id: ScopeId) {
        self.ensure_drop_safety(id);

        if let Some(root) = self.scopes[id.0].as_ref().try_root_node() {
            if let RenderReturn::Ready(node) = unsafe { root.extend_lifetime_ref() } {
                self.drop_scope_inner(node)
            }
        }
        if let Some(root) = unsafe { self.scopes[id.0].as_ref().previous_frame().try_load_node() } {
            if let RenderReturn::Ready(node) = unsafe { root.extend_lifetime_ref() } {
                self.drop_scope_inner(node)
            }
        }

        self.scopes[id.0].props.take();

        let scope = &mut self.scopes[id.0];

        // Drop all the hooks once the children are dropped
        // this means we'll drop hooks bottom-up
        for hook in scope.hook_list.get_mut().drain(..) {
            drop(unsafe { BumpBox::from_raw(hook) });
        }
    }

    fn drop_scope_inner(&mut self, node: &VNode) {
        node.clear_listeners();
        node.dynamic_nodes.iter().for_each(|node| match node {
            DynamicNode::Component(c) => {
                if let Some(f) = c.scope.get() {
                    self.drop_scope(f);
                }
                c.props.take();
            }
            DynamicNode::Fragment(nodes) => {
                nodes.iter().for_each(|node| self.drop_scope_inner(node))
            }
            DynamicNode::Placeholder(t) => {
                if let Some(id) = t.id.get() {
                    self.try_reclaim(id);
                }
            }
            DynamicNode::Text(t) => {
                if let Some(id) = t.id.get() {
                    self.try_reclaim(id);
                }
            }
        });

        for id in &node.root_ids {
            if id.0 != 0 {
                self.try_reclaim(id);
            }
        }
    }

    /// Descend through the tree, removing any borrowed props and listeners
    pub(crate) fn ensure_drop_safety(&self, scope_id: ScopeId) {
        let scope = &self.scopes[scope_id.0];

        // make sure we drop all borrowed props manually to guarantee that their drop implementation is called before we
        // run the hooks (which hold an &mut Reference)
        // recursively call ensure_drop_safety on all children
        let mut props = scope.borrowed_props.borrow_mut();
        props.drain(..).for_each(|comp| {
            let comp = unsafe { &*comp };
            match comp.scope.get() {
                Some(child) if child != scope_id => self.ensure_drop_safety(child),
                _ => (),
            }
            if let Ok(mut props) = comp.props.try_borrow_mut() {
                *props = None;
            }
        });

        // Now that all the references are gone, we can safely drop our own references in our listeners.
        let mut listeners = scope.attributes_to_drop.borrow_mut();
        listeners.drain(..).for_each(|listener| {
            let listener = unsafe { &*listener };
            match &listener.value {
                AttributeValue::Listener(l) => {
                    _ = l.take();
                }
                AttributeValue::Any(a) => {
                    _ = a.take();
                }
                _ => (),
            }
        });
    }
}

impl ElementPath {
    pub(crate) fn is_ascendant(&self, big: &&[u8]) -> bool {
        match *self {
            ElementPath::Deep(small) => small.len() <= big.len() && small == &big[..small.len()],
            ElementPath::Root(r) => big.len() == 1 && big[0] == r as u8,
        }
    }
}

impl PartialEq<&[u8]> for ElementPath {
    fn eq(&self, other: &&[u8]) -> bool {
        match *self {
            ElementPath::Deep(deep) => deep.eq(*other),
            ElementPath::Root(r) => other.len() == 1 && other[0] == r as u8,
        }
    }
}
