//! Enable the dom to participate in styling by servo
//!

use crate::events::EventData;
use crate::events::HitResult;
use crate::node::Node;
use crate::node::NodeData;
use atomic_refcell::{AtomicRef, AtomicRefMut};
use html5ever::LocalNameStaticSet;
use html5ever::NamespaceStaticSet;
use html5ever::{local_name, LocalName, Namespace};
use selectors::{
    attr::{AttrSelectorOperation, AttrSelectorOperator, NamespaceConstraint},
    matching::{ElementSelectorFlags, MatchingContext, VisitedHandlingMode},
    sink::Push,
    Element, OpaqueElement,
};
use slab::Slab;
use std::sync::atomic::Ordering;
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::color::AbsoluteColor;
use style::properties::{Importance, PropertyDeclaration};
use style::rule_tree::CascadeLevel;
use style::selector_parser::PseudoElement;
use style::stylesheets::layer_rule::LayerOrder;
use style::values::computed::text::TextAlign as StyloTextAlign;
use style::values::computed::Display;
use style::values::computed::Percentage;
use style::values::specified::box_::DisplayInside;
use style::values::specified::box_::DisplayOutside;
use style::values::AtomString;
use style::CaseSensitivityExt;
use style::{
    context::{
        QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters,
        SharedStyleContext, StyleContext,
    },
    dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot},
    properties::PropertyDeclarationBlock,
    selector_parser::{NonTSPseudoClass, SelectorImpl},
    servo_arc::{Arc, ArcBorrow},
    shared_lock::{Locked, SharedRwLock},
    traversal::{DomTraversal, PerLevelTraversalData},
    values::{AtomIdent, GenericAtomIdent},
    Atom,
};
use style_dom::ElementState;
use winit::event::Modifiers;

/// A handle to a node that Servo's style traits are implemented against
///
/// Since BlitzNodes are not persistent (IE we don't keep the pointers around between frames), we choose to just implement
/// the tree structure in the nodes themselves, and temporarily give out pointers during the layout phase.
#[derive(Clone, Copy, Debug)]
pub struct Handle<'a> {
    pub node: &'a Node,
    pub tree: &'a Slab<Node>,
}

impl Handle<'_> {
    pub fn get(&self, id: usize) -> Self {
        Self {
            node: &self.tree[id],
            tree: self.tree,
        }
    }

    // Get the index of the current node in the parents child list
    pub fn child_index(&self) -> Option<usize> {
        self.node.parent.and_then(|parent_id| {
            self.tree[parent_id]
                .children
                .iter()
                .position(|id| *id == self.node.id)
        })
    }

    pub fn forward(&self, n: usize) -> Option<Self> {
        let child_idx = self.child_index().unwrap_or(0);
        self.tree[self.node.parent?]
            .children
            .get(child_idx + n)
            .map(|id| Self {
                node: &self.tree[*id],
                tree: self.tree,
            })
    }

    pub fn backward(&self, n: usize) -> Option<Self> {
        let child_idx = self.child_index().unwrap_or(0);
        self.tree[self.node.parent?]
            .children
            .get(child_idx - n)
            .map(|id| Self {
                node: &self.tree[*id],
                tree: self.tree,
            })
    }

    /// Computes the Document-relative coordinates of the Node
    pub fn absolute_position(&self, x: f32, y: f32) -> taffy::Point<f32> {
        let x = x + self.node.final_layout.location.x - self.node.scroll_offset.x as f32;
        let y = y + self.node.final_layout.location.y - self.node.scroll_offset.y as f32;

        self.node
            .layout_parent
            .get()
            .map(|i| {
                Self {
                    node: &self.tree[i],
                    tree: self.tree,
                }
                .absolute_position(x, y)
            })
            .unwrap_or(taffy::Point { x, y })
    }

    /// Creates a synteh
    pub fn synthetic_click_event(&self, mods: Modifiers) -> EventData {
        let absolute_position = self.absolute_position(0.0, 0.0);
        let x = absolute_position.x + (self.node.final_layout.size.width / 2.0);
        let y = absolute_position.y + (self.node.final_layout.size.height / 2.0);

        EventData::Click { x, y, mods }
    }

    /// Takes an (x, y) position (relative to the *parent's* top-left corner) and returns:
    ///    - None if the position is outside of this node's bounds
    ///    - Some(HitResult) if the position is within the node but doesn't match any children
    ///    - The result of recursively calling child.hit() on the the child element that is
    ///      positioned at that position if there is one.
    ///
    /// TODO: z-index
    /// (If multiple children are positioned at the position then a random one will be recursed into)
    pub fn hit(&self, x: f32, y: f32) -> Option<HitResult> {
        let x = x - self.node.final_layout.location.x + self.node.scroll_offset.x as f32;
        let y = y - self.node.final_layout.location.y + self.node.scroll_offset.y as f32;

        let size = self.node.final_layout.size;

        if x < 0.0
            || x > size.width + self.node.scroll_offset.x as f32
            || y < 0.0
            || y > size.height + self.node.scroll_offset.y as f32
        {
            return None;
        }

        // Call `.hit()` on each child in turn. If any return `Some` then return that value. Else return `Some(self.id).
        self.node
            .layout_children
            .borrow()
            .iter()
            .flatten()
            .find_map(|&id| self.get(id).hit(x, y))
            .or(Some(HitResult {
                node_id: self.node.id,
                x,
                y,
            }))
    }

    pub fn text_content(&self) -> String {
        let mut out = String::new();
        self.write_text_content(&mut out);
        out
    }

    fn write_text_content(&self, out: &mut String) {
        match &self.node.raw_dom_data {
            NodeData::Text(data) => {
                out.push_str(&data.content);
            }
            NodeData::Element(..) | NodeData::AnonymousBlock(..) => {
                for child_id in &self.node.children {
                    self.get(*child_id).write_text_content(out);
                }
            }
            _ => {}
        }
    }

    /// Returns true if this node, or any of its children, is a block.
    pub fn is_or_contains_block(&self) -> bool {
        let display = self.node.display_style().unwrap_or(Display::inline());

        match display.outside() {
            DisplayOutside::None => false,
            DisplayOutside::Block => true,
            _ => {
                if display.inside() == DisplayInside::Flow {
                    self.node
                        .children
                        .iter()
                        .copied()
                        .any(|child_id| self.get(child_id).is_or_contains_block())
                } else {
                    false
                }
            }
        }
    }

    pub fn print_tree(&self, level: usize) {
        println!(
            "{} {} {:?} {} {:?}",
            "  ".repeat(level),
            self.node.id,
            self.node.parent,
            self.node.node_debug_str().replace('\n', ""),
            self.node.children
        );

        for child_id in &self.node.children {
            let child = self.get(*child_id);
            child.print_tree(level + 1)
        }
    }
}

impl PartialEq for Handle<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}

// TODO (Matt)
impl Eq for Handle<'_> {}

impl TDocument for Handle<'_> {
    type ConcreteNode = Self;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> QuirksMode {
        QuirksMode::NoQuirks
    }

    fn shared_lock(&self) -> &SharedRwLock {
        &self.node.guard
    }
}

impl NodeInfo for Handle<'_> {
    fn is_element(&self) -> bool {
        self.node.is_element()
    }

    fn is_text_node(&self) -> bool {
        self.node.is_text_node()
    }
}

impl TShadowRoot for Handle<'_> {
    type ConcreteNode = Self;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        todo!("Shadow roots not implemented")
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        todo!("Shadow roots not implemented")
    }
}

// components/styleaapper.rs:
impl TNode for Handle<'_> {
    type ConcreteElement = Self;
    type ConcreteDocument = Self;
    type ConcreteShadowRoot = Self;

    fn parent_node(&self) -> Option<Self> {
        self.node.parent.map(|id| self.get(id))
    }

    fn first_child(&self) -> Option<Self> {
        self.node.children.first().map(|id| self.get(*id))
    }

    fn last_child(&self) -> Option<Self> {
        self.node.children.last().map(|id| self.get(*id))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.backward(1)
    }

    fn next_sibling(&self) -> Option<Self> {
        self.forward(1)
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        self.get(1)
    }

    fn is_in_document(&self) -> bool {
        true
    }

    // I think this is the same as parent_node only in the cases when the direct parent is not a real element, forcing us
    // to travel upwards
    //
    // For the sake of this demo, we're just going to return the parent node ann
    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        self.parent_node().and_then(|node| node.as_element())
    }

    fn opaque(&self) -> OpaqueNode {
        OpaqueNode(self.node.id)
    }

    fn debug_id(self) -> usize {
        self.node.id
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        match self.node.raw_dom_data {
            NodeData::Element { .. } => Some(*self),
            _ => None,
        }
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        match self.node.raw_dom_data {
            NodeData::Document { .. } => Some(*self),
            _ => None,
        }
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        todo!("Shadow roots aren't real, yet")
    }
}

impl selectors::Element for Handle<'_> {
    type Impl = SelectorImpl;

    fn opaque(&self) -> selectors::OpaqueElement {
        // FIXME: this is wrong in the case where pushing new elements casuses reallocations.
        // We should see if selectors will accept a PR that allows creation from a usize
        OpaqueElement::new(self)
    }

    fn parent_element(&self) -> Option<Self> {
        TElement::traversal_parent(self)
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        matches!(self.node.raw_dom_data, NodeData::AnonymousBlock(_))
    }

    // These methods are implemented naively since we only threaded real nodes and not fake nodes
    // we should try and use `find` instead of this foward/backward stuff since its ugly and slow
    fn prev_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.backward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.forward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn first_element_child(&self) -> Option<Self> {
        let mut children = self.dom_children();
        children.find(|child| child.is_element())
    }

    fn is_html_element_in_html_document(&self) -> bool {
        true // self.has_namespace(ns!(html))
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.node.raw_dom_data.is_element_with_tag_name(local_name)
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.node.element_data().expect("Not an element").name.ns == *ns
    }

    fn is_same_type(&self, _other: &Self) -> bool {
        // FIXME: implementing this correctly currently triggers a debug_assert ("Invalid cache") in selectors
        //self.local_name() == other.local_name() && self.namespace() == other.namespace()
        false
    }

    fn attr_matches(
        &self,
        _ns: &NamespaceConstraint<&GenericAtomIdent<NamespaceStaticSet>>,
        local_name: &GenericAtomIdent<LocalNameStaticSet>,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        let Some(attr_value) = self.node.raw_dom_data.attr(local_name.0.clone()) else {
            return false;
        };

        match operation {
            AttrSelectorOperation::Exists => true,
            AttrSelectorOperation::WithValue {
                operator,
                case_sensitivity: _,
                value,
            } => {
                let value = value.as_ref();

                // TODO: case sensitivity
                match operator {
                    AttrSelectorOperator::Equal => attr_value == value,
                    AttrSelectorOperator::Includes => attr_value
                        .split_ascii_whitespace()
                        .any(|word| word == value),
                    AttrSelectorOperator::DashMatch => {
                        // Represents elements with an attribute name of attr whose value can be exactly value
                        // or can begin with value immediately followed by a hyphen, - (U+002D)
                        attr_value.starts_with(value)
                            && (attr_value.len() == value.len()
                                || attr_value.chars().nth(value.len()) == Some('-'))
                    }
                    AttrSelectorOperator::Prefix => attr_value.starts_with(value),
                    AttrSelectorOperator::Substring => attr_value.contains(value),
                    AttrSelectorOperator::Suffix => attr_value.ends_with(value),
                }
            }
        }
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &<Self::Impl as selectors::SelectorImpl>::NonTSPseudoClass,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        match *pseudo_class {
            NonTSPseudoClass::Active => false,
            NonTSPseudoClass::AnyLink => self
                .node
                .raw_dom_data
                .downcast_element()
                .map(|elem| {
                    (elem.name.local == local_name!("a") || elem.name.local == local_name!("area"))
                        && elem.attr(local_name!("href")).is_some()
                })
                .unwrap_or(false),
            NonTSPseudoClass::Checked => self
                .node
                .raw_dom_data
                .downcast_element()
                .and_then(|elem| elem.checkbox_input_checked())
                .unwrap_or(false),
            NonTSPseudoClass::Valid => false,
            NonTSPseudoClass::Invalid => false,
            NonTSPseudoClass::Defined => false,
            NonTSPseudoClass::Disabled => false,
            NonTSPseudoClass::Enabled => false,
            NonTSPseudoClass::Focus => self.node.element_state.contains(ElementState::FOCUS),
            NonTSPseudoClass::FocusWithin => false,
            NonTSPseudoClass::FocusVisible => false,
            NonTSPseudoClass::Fullscreen => false,
            NonTSPseudoClass::Hover => self.node.element_state.contains(ElementState::HOVER),
            NonTSPseudoClass::Indeterminate => false,
            NonTSPseudoClass::Lang(_) => false,
            NonTSPseudoClass::CustomState(_) => false,
            NonTSPseudoClass::Link => self
                .node
                .raw_dom_data
                .downcast_element()
                .map(|elem| {
                    (elem.name.local == local_name!("a") || elem.name.local == local_name!("area"))
                        && elem.attr(local_name!("href")).is_some()
                })
                .unwrap_or(false),
            NonTSPseudoClass::PlaceholderShown => false,
            NonTSPseudoClass::ReadWrite => false,
            NonTSPseudoClass::ReadOnly => false,
            NonTSPseudoClass::ServoNonZeroBorder => false,
            NonTSPseudoClass::Target => false,
            NonTSPseudoClass::Visited => false,
            NonTSPseudoClass::Autofill => false,
            NonTSPseudoClass::Default => false,

            NonTSPseudoClass::InRange => false,
            NonTSPseudoClass::Modal => false,
            NonTSPseudoClass::Optional => false,
            NonTSPseudoClass::OutOfRange => false,
            NonTSPseudoClass::PopoverOpen => false,
            NonTSPseudoClass::Required => false,
            NonTSPseudoClass::UserInvalid => false,
            NonTSPseudoClass::UserValid => false,
        }
    }

    fn match_pseudo_element(
        &self,
        pe: &PseudoElement,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        match self.node.raw_dom_data {
            NodeData::AnonymousBlock(_) => *pe == PseudoElement::ServoAnonymousBox,
            _ => false,
        }
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        // Handle flags that apply to the element.
        let self_flags = flags.for_self();
        if !self_flags.is_empty() {
            *self.node.selector_flags.borrow_mut() |= self_flags;
        }

        // Handle flags that apply to the parent.
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty() {
            if let Some(parent) = self.parent_node() {
                *parent.node.selector_flags.borrow_mut() |= self_flags;
            }
        }
    }

    fn is_link(&self) -> bool {
        self.node
            .raw_dom_data
            .is_element_with_tag_name(&local_name!("a"))
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn has_id(
        &self,
        id: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        self.node
            .element_data()
            .and_then(|data| data.id.as_ref())
            .map(|id_attr| case_sensitivity.eq_atom(id_attr, id))
            .unwrap_or(false)
    }

    fn has_class(
        &self,
        search_name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        let class_attr = self.node.raw_dom_data.attr(local_name!("class"));
        if let Some(class_attr) = class_attr {
            // split the class attribute
            for pheme in class_attr.split_ascii_whitespace() {
                let atom = Atom::from(pheme);
                if case_sensitivity.eq_atom(&atom, search_name) {
                    return true;
                }
            }
        }

        false
    }

    fn imported_part(
        &self,
        _name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> Option<<Self::Impl as selectors::SelectorImpl>::Identifier> {
        None
    }

    fn is_part(&self, _name: &<Self::Impl as selectors::SelectorImpl>::Identifier) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        self.dom_children().next().is_none()
    }

    fn is_root(&self) -> bool {
        self.parent_node()
            .and_then(|parent| parent.parent_node())
            .is_none()
    }

    fn has_custom_state(
        &self,
        _name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> bool {
        false
    }

    fn add_element_unique_hashes(&self, _filter: &mut selectors::bloom::BloomFilter) -> bool {
        false
    }
}

impl<'a> TElement for Handle<'a> {
    type ConcreteNode = Handle<'a>;

    type TraversalChildrenIterator = Traverser<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn unopaque(opaque: OpaqueElement) -> Self {
        // FIXME: this is wrong in the case where pushing new elements casuses reallocations.
        // We should see if selectors will accept a PR that allows creation from a usize
        unsafe { *opaque.as_const_ptr() }
    }

    fn traversal_children(&self) -> style::dom::LayoutIterator<Self::TraversalChildrenIterator> {
        LayoutIterator(Traverser {
            // dom: self.tree(),
            parent: *self,
            child_index: 0,
        })
    }

    fn is_html_element(&self) -> bool {
        self.is_element()
    }

    // not implemented.....
    fn is_mathml_element(&self) -> bool {
        false
    }

    // need to check the namespace
    fn is_svg_element(&self) -> bool {
        false
    }

    fn style_attribute(&self) -> Option<ArcBorrow<Locked<PropertyDeclarationBlock>>> {
        self.node
            .element_data()
            .expect("Not an element")
            .style_attribute
            .as_ref()
            .map(|f| f.borrow_arc())
    }

    fn animation_rule(
        &self,
        _: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn transition_rule(
        &self,
        _context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn state(&self) -> ElementState {
        self.node.element_state
    }

    fn has_part_attr(&self) -> bool {
        false
    }

    fn exports_any_part(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&style::Atom> {
        self.node.element_data().and_then(|data| data.id.as_ref())
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&style::values::AtomIdent),
    {
        let class_attr = self.node.raw_dom_data.attr(local_name!("class"));
        if let Some(class_attr) = class_attr {
            // split the class attribute
            for pheme in class_attr.split_ascii_whitespace() {
                let atom = Atom::from(pheme); // interns the string
                callback(AtomIdent::cast(&atom));
            }
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        if let Some(attrs) = self.node.raw_dom_data.attrs() {
            for attr in attrs.iter() {
                callback(&GenericAtomIdent(attr.name.local.clone()));
            }
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        true
    }

    fn has_snapshot(&self) -> bool {
        self.node.has_snapshot
    }

    fn handled_snapshot(&self) -> bool {
        self.node.snapshot_handled.load(Ordering::SeqCst)
    }

    unsafe fn set_handled_snapshot(&self) {
        self.node.snapshot_handled.store(true, Ordering::SeqCst);
    }

    unsafe fn set_dirty_descendants(&self) {}

    unsafe fn unset_dirty_descendants(&self) {}

    fn store_children_to_process(&self, _n: isize) {
        unimplemented!()
    }

    fn did_process_child(&self) -> isize {
        unimplemented!()
    }

    unsafe fn ensure_data(&self) -> AtomicRefMut<style::data::ElementData> {
        let mut stylo_data = self.node.stylo_element_data.borrow_mut();
        if stylo_data.is_none() {
            *stylo_data = Some(Default::default());
        }
        AtomicRefMut::map(stylo_data, |sd| sd.as_mut().unwrap())
    }

    unsafe fn clear_data(&self) {
        *self.node.stylo_element_data.borrow_mut() = None;
    }

    fn has_data(&self) -> bool {
        self.node.stylo_element_data.borrow().is_some()
    }

    fn borrow_data(&self) -> Option<AtomicRef<style::data::ElementData>> {
        let stylo_data = self.node.stylo_element_data.borrow();
        if stylo_data.is_some() {
            Some(AtomicRef::map(stylo_data, |sd| sd.as_ref().unwrap()))
        } else {
            None
        }
    }

    fn mutate_data(&self) -> Option<AtomicRefMut<style::data::ElementData>> {
        let stylo_data = self.node.stylo_element_data.borrow_mut();
        if stylo_data.is_some() {
            Some(AtomicRefMut::map(stylo_data, |sd| sd.as_mut().unwrap()))
        } else {
            None
        }
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        false
    }

    fn has_animations(&self, _context: &SharedStyleContext) -> bool {
        false
    }

    fn has_css_animations(
        &self,
        _context: &SharedStyleContext,
        _pseudo_element: Option<style::selector_parser::PseudoElement>,
    ) -> bool {
        false
    }

    fn has_css_transitions(
        &self,
        _context: &SharedStyleContext,
        _pseudo_element: Option<style::selector_parser::PseudoElement>,
    ) -> bool {
        false
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn lang_attr(&self) -> Option<style::selector_parser::AttrValue> {
        None
    }

    fn match_element_lang(
        &self,
        _override_lang: Option<Option<style::selector_parser::AttrValue>>,
        _value: &style::selector_parser::Lang,
    ) -> bool {
        false
    }

    fn is_html_document_body_element(&self) -> bool {
        // Check node is a <body> element
        let is_body_element = self
            .node
            .raw_dom_data
            .is_element_with_tag_name(&local_name!("body"));

        // If it isn't then return early
        if !is_body_element {
            return false;
        }

        // If it is then check if it is a child of the root (<html>) element
        let root_node = &self.get(0);
        let root_element = TDocument::as_node(root_node).first_element_child().unwrap();
        root_element.node.children.contains(&self.node.id)
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: Push<style::applicable_declarations::ApplicableDeclarationBlock>,
    {
        let Some(elem) = self.node.raw_dom_data.downcast_element() else {
            return;
        };

        let mut push_style = |decl: PropertyDeclaration| {
            hints.push(ApplicableDeclarationBlock::from_declarations(
                Arc::new(
                    self.node
                        .guard
                        .wrap(PropertyDeclarationBlock::with_one(decl, Importance::Normal)),
                ),
                CascadeLevel::PresHints,
                LayerOrder::root(),
            ));
        };

        fn parse_color_attr(value: &str) -> Option<(u8, u8, u8, f32)> {
            if !value.starts_with('#') {
                return None;
            }

            let value = &value[1..];
            if value.len() == 3 {
                let r = u8::from_str_radix(&value[0..1], 16).ok()?;
                let g = u8::from_str_radix(&value[1..2], 16).ok()?;
                let b = u8::from_str_radix(&value[2..3], 16).ok()?;
                return Some((r, g, b, 1.0));
            }

            if value.len() == 6 {
                let r = u8::from_str_radix(&value[0..2], 16).ok()?;
                let g = u8::from_str_radix(&value[2..4], 16).ok()?;
                let b = u8::from_str_radix(&value[4..6], 16).ok()?;
                return Some((r, g, b, 1.0));
            }

            None
        }

        fn parse_size_attr(value: &str) -> Option<style::values::specified::LengthPercentage> {
            use style::values::specified::{AbsoluteLength, LengthPercentage, NoCalcLength};
            if let Some(value) = value.strip_suffix("px") {
                let val: f32 = value.parse().ok()?;
                return Some(LengthPercentage::Length(NoCalcLength::Absolute(
                    AbsoluteLength::Px(val),
                )));
            }

            if let Some(value) = value.strip_suffix("%") {
                let val: f32 = value.parse().ok()?;
                return Some(LengthPercentage::Percentage(Percentage(val / 100.0)));
            }

            let val: f32 = value.parse().ok()?;
            Some(LengthPercentage::Length(NoCalcLength::Absolute(
                AbsoluteLength::Px(val),
            )))
        }

        for attr in elem.attrs() {
            let name = &attr.name.local;
            let value = attr.value.as_str();

            if *name == local_name!("align") {
                use style::values::specified::TextAlign;
                let keyword = match value {
                    "left" => Some(StyloTextAlign::MozLeft),
                    "right" => Some(StyloTextAlign::MozRight),
                    "center" => Some(StyloTextAlign::MozCenter),
                    _ => None,
                };

                if let Some(keyword) = keyword {
                    push_style(PropertyDeclaration::TextAlign(TextAlign::Keyword(keyword)));
                }
            }

            if *name == local_name!("width") {
                if let Some(width) = parse_size_attr(value) {
                    use style::values::generics::{length::Size, NonNegative};
                    push_style(PropertyDeclaration::Width(Size::LengthPercentage(
                        NonNegative(width),
                    )));
                }
            }

            if *name == local_name!("height") {
                if let Some(height) = parse_size_attr(value) {
                    use style::values::generics::{length::Size, NonNegative};
                    push_style(PropertyDeclaration::Height(Size::LengthPercentage(
                        NonNegative(height),
                    )));
                }
            }

            if *name == local_name!("bgcolor") {
                use style::values::specified::Color;
                if let Some((r, g, b, a)) = parse_color_attr(value) {
                    push_style(PropertyDeclaration::BackgroundColor(
                        Color::from_absolute_color(AbsoluteColor::srgb_legacy(r, g, b, a)),
                    ));
                }
            }
        }
    }

    fn local_name(&self) -> &LocalName {
        &self.node.element_data().expect("Not an element").name.local
    }

    fn namespace(&self) -> &Namespace {
        &self.node.element_data().expect("Not an element").name.ns
    }

    fn query_container_size(
        &self,
        _display: &style::values::specified::Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        // FIXME: Implement container queries. For now this effectively disables them without panicking.
        Default::default()
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        todo!()
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.node.selector_flags.borrow().contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        self.node
            .selector_flags
            .borrow()
            .intersection(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING)
    }

    // fn update_animations(
    //     &self,
    //     before_change_style: Option<Arc<ComputedValues>>,
    //     tasks: style::context::UpdateAnimationsTasks,
    // ) {
    //     todo!()
    // }

    // fn process_post_animation(&self, tasks: style::context::PostAnimationTasks) {
    //     todo!()
    // }

    // fn needs_transitions_update(
    //     &self,
    //     before_change_style: &ComputedValues,
    //     after_change_style: &ComputedValues,
    // ) -> bool {
    //     todo!()
    // }
}

pub struct Traverser<'a> {
    // dom: &'a Slab<Node>,
    parent: Handle<'a>,
    child_index: usize,
}

impl<'a> Iterator for Traverser<'a> {
    type Item = Handle<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let node_id = self.parent.node.children.get(self.child_index)?;
        let node = self.parent.tree.get(*node_id)?;

        self.child_index += 1;

        Some(Handle {
            node,
            tree: self.parent.tree,
        })
    }
}

impl std::hash::Hash for Handle<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_usize(self.node.id)
    }
}

/// Handle custom painters like images for layouting
///
/// todo: actually implement this
pub struct RegisteredPaintersImpl;
impl RegisteredSpeculativePainters for RegisteredPaintersImpl {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

use style::traversal::recalc_style_at;

pub struct RecalcStyle<'a> {
    context: SharedStyleContext<'a>,
}

impl<'a> RecalcStyle<'a> {
    pub fn new(context: SharedStyleContext<'a>) -> Self {
        RecalcStyle { context }
    }
}

#[allow(unsafe_code)]
impl<E> DomTraversal<E> for RecalcStyle<'_>
where
    E: TElement,
{
    fn process_preorder<F: FnMut(E::ConcreteNode)>(
        &self,
        traversal_data: &PerLevelTraversalData,
        context: &mut StyleContext<E>,
        node: E::ConcreteNode,
        note_child: F,
    ) {
        // Don't process textnodees in this traversal
        if node.is_text_node() {
            return;
        }

        let el = node.as_element().unwrap();
        // let mut data = el.mutate_data().unwrap();
        let mut data = unsafe { el.ensure_data() };
        recalc_style_at(self, traversal_data, context, el, &mut data, note_child);

        // Gets set later on
        unsafe { el.unset_dirty_descendants() }
    }

    #[inline]
    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(&self, _style_context: &mut StyleContext<E>, _node: E::ConcreteNode) {
        panic!("this should never be called")
    }

    #[inline]
    fn shared_context(&self) -> &SharedStyleContext {
        &self.context
    }
}

#[test]
fn assert_size_of_equals() {
    // use std::mem;

    // fn assert_layout<E>() {
    //     assert_eq!(
    //         mem::size_of::<SharingCache<E>>(),
    //         mem::size_of::<TypelessSharingCache>()
    //     );
    //     assert_eq!(
    //         mem::align_of::<SharingCache<E>>(),
    //         mem::align_of::<TypelessSharingCache>()
    //     );
    // }

    // let size = mem::size_of::<StyleSharingCandidate<BlitzNode>>();
    // dbg!(size);
}

#[test]
fn parse_inline() {
    // let attrs = style::attr::AttrValue::from_serialized_tokenlist(
    //     r#"visibility: hidden; left: 1306.5px; top: 50px; display: none;"#.to_string(),
    // );

    // let val = CSSInlineStyleDeclaration();
}