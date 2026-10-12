//! The XPath 1.0 data model, and the two documents it is built from.
//!
//! The scraping feed kinds — HTML+XPath and XML+XPath — are configured with XPath expressions the
//! user wrote, so evaluating them is not optional. `xpath-eval` supplies the evaluator but not a
//! tree, which is what this module is: one arena that both an HTML parse (via `scraper`) and an XML
//! parse (via `quick-xml`) are copied into, so the expressions evaluate the same way against either.
//!
//! Having one tree rather than two is the point. A scraper kind configured as HTML+XPath and one
//! configured as XML+XPath differ only in how lenient the parser is about the bytes; if each had its
//! own data model, `normalize-space()`, `position()` and every axis would have to behave identically
//! in two implementations, and the way that fails is silently — a selector that matches one and not
//! the other, with no error to point at.
//!
//! The arena is built depth-first with each node's namespace nodes, then its attribute nodes, then
//! its children. That ordering is what the XPath data model specifies for document order, and it
//! makes document order an index comparison instead of a traversal, which matters because predicates
//! compare node pairs.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::LazyLock;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use scraper::{Html, Node as HtmlNode};
use xpath_eval::{
    evaluate, parse, Document, EvalError, EvaluationContext, ExpandedName, Expr, Node, NodeKind,
    Value,
};

use crate::error::FeedError;
use crate::parse::xml::{attrs_prefixed, split};

static BASE_HREF: LazyLock<Expr> =
    LazyLock::new(|| parse("//base/@href").expect("`//base/@href` is valid XPath"));

/// Elements HTML does not close, so the serializer must not invent an end tag for them.
const VOID_ELEMENTS: [&str; 13] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track",
];

/// Whether the document was parsed as HTML or as XML, which decides how it is written back out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocKind {
    Html,
    Xml,
}

/// One node of the arena.
#[derive(Debug)]
struct NodeData {
    kind: NodeKind,
    name: Option<ExpandedName>,
    /// The text of a text, comment, attribute or namespace node, or the data of a processing
    /// instruction.
    text: Option<String>,
    parent: Option<usize>,
    children: Vec<usize>,
    /// Not part of `children`: in the XPath data model an attribute's parent is its element but it is
    /// never a child of it.
    attributes: Vec<usize>,
    namespaces: Vec<usize>,
}

impl NodeData {
    fn new(kind: NodeKind, parent: Option<usize>) -> Self {
        NodeData {
            kind,
            name: None,
            text: None,
            parent,
            children: Vec::new(),
            attributes: Vec::new(),
            namespaces: Vec::new(),
        }
    }
}

/// A parsed document that XPath expressions can be evaluated against.
#[derive(Debug)]
pub struct Doc {
    arena: Box<[NodeData]>,
    kind: DocKind,
    /// Every prefix→URI declaration in the document, for resolving the prefixes used in an
    /// *expression*.
    ///
    /// A single flat map is an approximation: XPath resolves an expression's prefixes against the
    /// namespace context of the evaluation, which for a document-wide expression means the root's.
    /// When a document declares the same prefix twice with different URIs, the first declaration
    /// wins. That is wrong in principle and right for every feed in practice, and the alternative —
    /// threading a scope through every evaluation — is not something the evaluator's hook API can
    /// express.
    prefixes: HashMap<String, String>,
}

/// A node of a [`Doc`], as an XPath 1.0 data-model node.
#[derive(Debug, Clone, Copy)]
pub struct NodeRef<'a> {
    arena: &'a [NodeData],
    index: usize,
}

/// Two handles are the same node when they index the same arena.
///
/// Implemented by hand rather than derived: deriving would require every `NodeData` to be `Eq`, and
/// a node's identity is its position, not its contents.
impl PartialEq for NodeRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && std::ptr::eq(self.arena.as_ptr(), other.arena.as_ptr())
    }
}

impl Eq for NodeRef<'_> {}

impl Doc {
    /// Parses `html` the way a browser would: unclosed tags, misnested markup and all.
    ///
    /// Element names come back lowercased, which is what HTML means and what `DOMDocument::loadHTML`
    /// does, so `//DIV` and `//div` behave the same here as they do in PHP FreshRSS.
    pub fn from_html(html: &str) -> Doc {
        let parsed = Html::parse_document(html);
        let mut builder = Builder::new(DocKind::Html);
        // An explicit stack rather than recursion: the tree is built from untrusted bytes, and a
        // document nested ten thousand deep must not overflow.
        // The stack's element type is the parser's own node handle, which is not a type this crate
        // can name without depending on the tree library directly; inference is enough.
        let mut stack = vec![(parsed.tree.root(), builder.root())];
        while let Some((node, parent)) = stack.pop() {
            match node.value() {
                // Neither is in the XPath data model; their children belong to the same parent.
                HtmlNode::Document | HtmlNode::Fragment => {
                    stack.extend(node.children().rev().map(|child| (child, parent)));
                }
                HtmlNode::Element(element) => {
                    let index = builder.element(parent, element.name(), None);
                    for (name, value) in element.attrs() {
                        builder.attribute(index, name, None, value);
                    }
                    stack.extend(node.children().rev().map(|child| (child, index)));
                }
                HtmlNode::Text(text) => {
                    builder.text(parent, text);
                }
                HtmlNode::Comment(comment) => {
                    builder.comment(parent, comment);
                }
                HtmlNode::ProcessingInstruction(pi) => {
                    builder.instruction(parent, &pi.target, &pi.data);
                }
                // A doctype is not a node in the XPath data model.
                HtmlNode::Doctype(_) => {}
            }
        }
        builder.finish()
    }

    /// Parses `xml` strictly.
    ///
    /// Namespaces are resolved: `xmlns:atom="…"` on any ancestor makes `//atom:entry` match, which is
    /// the difference between an XML+XPath feed working and it silently finding nothing.
    ///
    /// # Errors
    ///
    /// [`FeedError::Parse`] when the bytes are not well-formed XML. There is no recovery mode here,
    /// unlike the HTML path: an XML document that does not parse has no meaning to recover.
    pub fn from_xml(xml: &str) -> Result<Doc, FeedError> {
        // A default-configured reader, not the lenient one the feed parsers use: `loadXML` rejects a
        // malformed document and so does this. Guessing at a structure the bytes do not have would
        // make a user's XPath expressions match the wrong things.
        let mut r = Reader::from_str(xml);
        let mut builder = Builder::new(DocKind::Xml);
        // Arena indices of the open elements, and the namespace scopes they introduced.
        let mut open: Vec<usize> = vec![builder.root()];
        let mut scopes: Vec<HashMap<String, String>> = vec![HashMap::new()];
        // The reader reports an entity reference as its own event, so the text around it arrives in
        // pieces. The data model wants one text node, and merging here is what makes `&amp;` survive
        // instead of silently vanishing.
        let mut pending = String::new();

        loop {
            let parent = *open.last().unwrap_or(&0);
            match r.read_event() {
                Ok(Event::Start(e)) => {
                    flush(&mut builder, parent, &mut pending);
                    let index = open_element(&mut builder, &e, parent, &mut scopes);
                    open.push(index);
                }
                Ok(Event::Empty(e)) => {
                    flush(&mut builder, parent, &mut pending);
                    open_element(&mut builder, &e, parent, &mut scopes);
                    scopes.pop();
                }
                Ok(Event::End(_)) => {
                    flush(&mut builder, parent, &mut pending);
                    if open.len() > 1 {
                        open.pop();
                        scopes.pop();
                    }
                }
                Ok(Event::Text(t)) => pending.push_str(&t),
                // CDATA content is literal by definition, so it is not unescaped.
                Ok(Event::CData(t)) => pending.push_str(&t),
                Ok(Event::GeneralRef(reference)) => {
                    pending.push_str(&resolve_reference(&reference))
                }
                Ok(Event::Comment(t)) => {
                    flush(&mut builder, parent, &mut pending);
                    builder.comment(parent, &t);
                }
                Ok(Event::PI(t)) => {
                    flush(&mut builder, parent, &mut pending);
                    let (target, data) = split_instruction(&t);
                    builder.instruction(parent, target, data);
                }
                // The XML declaration and a doctype are not nodes in the data model.
                Ok(Event::Eof) => {
                    // The reader has no notion of an element left open at end of input, so this is
                    // checked here. `loadXML` rejects such a document and so does this: a truncated
                    // page would otherwise scrape a subset of its articles and report success.
                    if open.len() > 1 {
                        return Err(FeedError::Parse(format!(
                            "not well-formed XML: {} element(s) left open at end of input",
                            open.len() - 1
                        )));
                    }
                    flush(&mut builder, parent, &mut pending);
                    break;
                }
                Err(err) => {
                    return Err(FeedError::Parse(format!("not well-formed XML: {err}")));
                }
                _ => {}
            }
        }
        Ok(builder.finish())
    }

    /// The document's root node.
    pub fn root(&self) -> NodeRef<'_> {
        self.node(0)
    }

    /// A node by its arena index.
    ///
    /// Indexes are dense and stable for the life of the `Doc`, which is what lets a caller hold on to
    /// the nodes an expression matched and come back to them after evaluating another expression.
    pub fn node(&self, index: usize) -> NodeRef<'_> {
        NodeRef {
            arena: &self.arena,
            index,
        }
    }

    /// Compiles an XPath expression.
    ///
    /// # Errors
    ///
    /// [`FeedError::BadConfig`] when the expression is not valid XPath. That is a configuration error
    /// the user can fix, not a document error, and it must be distinguishable from "matched nothing" —
    /// otherwise a typo looks like a page that changed.
    pub fn compile(expression: &str) -> Result<Expr, FeedError> {
        parse(expression)
            .map_err(|err| FeedError::BadConfig(format!("invalid XPath expression: {err}")))
    }

    /// Evaluates an expression with `context` as the context node.
    ///
    /// # Errors
    ///
    /// [`FeedError::BadConfig`] when the expression names a function or variable that does not exist.
    pub fn evaluate<'a>(
        &'a self,
        expr: &Expr,
        context: NodeRef<'a>,
    ) -> Result<Value<NodeRef<'a>>, FeedError> {
        let lookup = |prefix: &str| self.prefixes.get(prefix).cloned();
        let mut ctx = EvaluationContext::new(context);
        ctx.namespaces = Some(&lookup);
        evaluate(expr, &ctx).map_err(|err| match err {
            EvalError::UnknownFunction(name) => FeedError::BadConfig(match name.prefix {
                Some(prefix) => format!(
                    "unknown XPath function {prefix}:{}() in the expression",
                    name.local
                ),
                None => format!("unknown XPath function {}() in the expression", name.local),
            }),
            other => FeedError::BadConfig(format!(
                "the XPath expression could not be evaluated: {other:?}"
            )),
        })
    }

    /// Writes a node back out as markup, the way `saveHTML`/`saveXML` does.
    ///
    /// Used for `itemContent` when the expression matches nodes rather than a string: a scraped
    /// article body is the *markup* of what matched, not its text, or every image and link in the
    /// article would be lost.
    pub fn serialize(&self, node: NodeRef<'_>) -> String {
        let mut out = String::new();
        // (open | close) markers on an explicit stack, for the same reason the builders use one.
        let mut steps: Vec<(usize, bool)> = vec![(node.index, true)];
        while let Some((index, opening)) = steps.pop() {
            let data = &self.arena[index];
            if !opening {
                if let Some(name) = &data.name {
                    out.push_str("</");
                    out.push_str(&name.local_name);
                    out.push('>');
                }
                continue;
            }
            match data.kind {
                NodeKind::Root => steps.extend(children_reversed(data).map(|i| (i, true))),
                NodeKind::Text => escape_text(&mut out, data.text.as_deref().unwrap_or_default()),
                NodeKind::Comment => {
                    out.push_str("<!--");
                    out.push_str(data.text.as_deref().unwrap_or_default());
                    out.push_str("-->");
                }
                NodeKind::ProcessingInstruction => {
                    let name = data
                        .name
                        .as_ref()
                        .map(|n| n.local_name.as_str())
                        .unwrap_or_default();
                    out.push_str("<?");
                    out.push_str(name);
                    let content = data.text.as_deref().unwrap_or_default();
                    if !content.is_empty() {
                        out.push(' ');
                        out.push_str(content);
                    }
                    out.push_str("?>");
                }
                NodeKind::Element => {
                    let name = data
                        .name
                        .as_ref()
                        .map(|n| n.local_name.as_str())
                        .unwrap_or_default();
                    out.push('<');
                    out.push_str(name);
                    for attribute in &data.attributes {
                        let Some(attribute) = self.arena.get(*attribute) else {
                            continue;
                        };
                        let Some(attribute_name) = &attribute.name else {
                            continue;
                        };
                        out.push(' ');
                        out.push_str(&attribute_name.local_name);
                        out.push_str("=\"");
                        escape_attribute(&mut out, attribute.text.as_deref().unwrap_or_default());
                        out.push('"');
                    }
                    let void = self.kind == DocKind::Html && VOID_ELEMENTS.contains(&name);
                    let empty = data.children.is_empty();
                    if void {
                        out.push('>');
                        steps.extend(children_reversed(data).map(|i| (i, true)));
                    } else if empty && self.kind == DocKind::Xml {
                        out.push_str("/>");
                    } else {
                        out.push('>');
                        steps.push((index, false));
                        steps.extend(children_reversed(data).map(|i| (i, true)));
                    }
                }
                // Attribute and namespace nodes are serialized as part of their element.
                _ => {}
            }
        }
        out
    }

    /// The first `<base href>` in the document, resolved against `url`.
    ///
    /// The scraped page's own base is what its relative links mean, and it is not necessarily the URL
    /// the page was fetched from.
    pub fn base_href(&self, url: &str) -> String {
        let Ok(Value::NodeSet(nodes)) = self.evaluate(&BASE_HREF, self.root()) else {
            return url.to_owned();
        };
        let Some(href) = nodes.first() else {
            return url.to_owned();
        };
        let href = href.string_value();
        let href = href.trim();
        if href.is_empty() {
            url.to_owned()
        } else {
            crate::sanitize::uri::absolutize(href, url)
        }
    }
}

/// Children in reverse, so pushing them onto a LIFO stack visits them in order.
fn children_reversed(data: &NodeData) -> impl Iterator<Item = usize> + '_ {
    data.children.iter().copied().rev()
}

impl<'a> NodeRef<'a> {
    fn at(self, index: usize) -> Self {
        NodeRef {
            arena: self.arena,
            index,
        }
    }

    /// Concatenates the descendant text nodes, iteratively.
    fn collect_text(self, out: &mut String) {
        let mut stack: Vec<usize> = self.arena[self.index]
            .children
            .iter()
            .copied()
            .rev()
            .collect();
        while let Some(index) = stack.pop() {
            let data = &self.arena[index];
            match data.kind {
                NodeKind::Text => out.push_str(data.text.as_deref().unwrap_or_default()),
                NodeKind::Element | NodeKind::Root => {
                    stack.extend(data.children.iter().copied().rev());
                }
                _ => {}
            }
        }
    }
}

impl Document for Doc {
    type N<'b>
        = NodeRef<'b>
    where
        Self: 'b;

    fn root(&self) -> Self::N<'_> {
        Doc::root(self)
    }
}

impl<'a> Node<'a> for NodeRef<'a> {
    fn kind(self) -> NodeKind {
        self.arena[self.index].kind
    }

    fn parent(self) -> Option<Self> {
        self.arena[self.index].parent.map(|index| self.at(index))
    }

    fn children(self) -> impl Iterator<Item = Self> + 'a {
        let indices = &self.arena[self.index].children;
        // Reborrowed for 'a: the slice outlives `self`, which is a copy of two words.
        let arena = self.arena;
        indices
            .iter()
            .copied()
            .map(move |index| NodeRef { arena, index })
    }

    fn attributes(self) -> impl Iterator<Item = Self> + 'a {
        let arena = self.arena;
        self.arena[self.index]
            .attributes
            .iter()
            .copied()
            .map(move |index| NodeRef { arena, index })
    }

    fn namespaces(self) -> impl Iterator<Item = Self> + 'a {
        let arena = self.arena;
        self.arena[self.index]
            .namespaces
            .iter()
            .copied()
            .map(move |index| NodeRef { arena, index })
    }

    fn expanded_name(self) -> Option<ExpandedName> {
        self.arena[self.index].name.clone()
    }

    fn string_value(self) -> String {
        let data = &self.arena[self.index];
        match data.kind {
            NodeKind::Root | NodeKind::Element => {
                let mut out = String::new();
                self.collect_text(&mut out);
                out
            }
            _ => data.text.clone().unwrap_or_default(),
        }
    }

    /// Index order *is* document order, because the builder is depth-first and adds a node's
    /// namespaces, attributes and children immediately after the node itself.
    fn document_order(self, other: Self) -> Ordering {
        self.index.cmp(&other.index)
    }
}

/// Builds a [`Doc`].
///
/// The call order is load-bearing: for each element, its namespaces, then its attributes, then its
/// children, and each child's subtree finished before the next sibling starts. Depth-first insertion
/// is what makes the arena index order equal to document order.
struct Builder {
    nodes: Vec<NodeData>,
    kind: DocKind,
    prefixes: HashMap<String, String>,
}

impl Builder {
    fn new(kind: DocKind) -> Self {
        let nodes = vec![NodeData::new(NodeKind::Root, None)];
        Builder {
            nodes,
            kind,
            prefixes: HashMap::new(),
        }
    }

    fn root(&self) -> usize {
        0
    }

    fn push(&mut self, data: NodeData) -> usize {
        let index = self.nodes.len();
        self.nodes.push(data);
        index
    }

    fn element(&mut self, parent: usize, local: &str, namespace: Option<&str>) -> usize {
        let mut data = NodeData::new(NodeKind::Element, Some(parent));
        data.name = Some(expanded(local, namespace));
        let index = self.push(data);
        self.nodes[parent].children.push(index);
        index
    }

    fn namespace(&mut self, owner: usize, prefix: &str, uri: &str) -> usize {
        let mut data = NodeData::new(NodeKind::Namespace, Some(owner));
        // The namespace node's name is the prefix (empty for the default declaration) and its
        // string-value is the URI, per §5.4.
        data.name = Some(expanded(prefix, None));
        data.text = Some(uri.to_owned());
        let index = self.push(data);
        self.nodes[owner].namespaces.push(index);
        if !prefix.is_empty() {
            self.prefixes
                .entry(prefix.to_owned())
                .or_insert_with(|| uri.to_owned());
        }
        index
    }

    fn attribute(
        &mut self,
        owner: usize,
        local: &str,
        namespace: Option<&str>,
        value: &str,
    ) -> usize {
        let mut data = NodeData::new(NodeKind::Attribute, Some(owner));
        data.name = Some(expanded(local, namespace));
        data.text = Some(value.to_owned());
        let index = self.push(data);
        self.nodes[owner].attributes.push(index);
        index
    }

    fn text(&mut self, parent: usize, text: &str) -> usize {
        let mut data = NodeData::new(NodeKind::Text, Some(parent));
        data.text = Some(text.to_owned());
        let index = self.push(data);
        self.nodes[parent].children.push(index);
        index
    }

    fn comment(&mut self, parent: usize, text: &str) -> usize {
        let mut data = NodeData::new(NodeKind::Comment, Some(parent));
        data.text = Some(text.to_owned());
        let index = self.push(data);
        self.nodes[parent].children.push(index);
        index
    }

    fn instruction(&mut self, parent: usize, target: &str, data: &str) -> usize {
        let mut node = NodeData::new(NodeKind::ProcessingInstruction, Some(parent));
        node.name = Some(expanded(target, None));
        node.text = Some(data.to_owned());
        let index = self.push(node);
        self.nodes[parent].children.push(index);
        index
    }

    fn finish(self) -> Doc {
        Doc {
            arena: self.nodes.into_boxed_slice(),
            kind: self.kind,
            prefixes: self.prefixes,
        }
    }
}

fn expanded(local: &str, namespace: Option<&str>) -> ExpandedName {
    ExpandedName {
        namespace_uri: namespace.map(str::to_owned),
        local_name: local.to_owned(),
    }
}

/// Opens an XML element: declarations first, then the element, then its namespace nodes, then its
/// attributes — the order the data model requires.
fn open_element(
    builder: &mut Builder,
    e: &BytesStart<'_>,
    parent: usize,
    scopes: &mut Vec<HashMap<String, String>>,
) -> usize {
    let attributes = attrs_prefixed(e);
    scopes.push(HashMap::new());
    for (name, value) in &attributes {
        match name.as_str() {
            "xmlns" => declare(scopes, "", value),
            _ => {
                if let Some(prefix) = name.strip_prefix("xmlns:") {
                    declare(scopes, prefix, value);
                }
            }
        }
    }

    let (prefix, local) = split(e.name().into_inner());
    let namespace = prefix.and_then(|prefix| resolve(scopes, prefix));
    let index = builder.element(parent, local, namespace.as_deref());

    for (name, value) in &attributes {
        if name == "xmlns" {
            builder.namespace(index, "", value);
        } else if let Some(declared) = name.strip_prefix("xmlns:") {
            builder.namespace(index, declared, value);
        }
    }
    for (name, value) in &attributes {
        if name == "xmlns" || name.starts_with("xmlns:") {
            continue;
        }
        // The default namespace does not apply to attributes: an unprefixed attribute is in no
        // namespace, which is why `@href` matches even inside an XHTML document with a default xmlns.
        let (prefix, local) = split(name);
        let namespace = prefix.and_then(|prefix| resolve(scopes, prefix));
        builder.attribute(index, local, namespace.as_deref(), value);
    }
    index
}

fn declare(scopes: &mut [HashMap<String, String>], prefix: &str, uri: &str) {
    if let Some(scope) = scopes.last_mut() {
        scope.insert(prefix.to_owned(), uri.to_owned());
    }
}

fn resolve(scopes: &[HashMap<String, String>], prefix: &str) -> Option<String> {
    scopes
        .iter()
        .rev()
        .find_map(|scope| scope.get(prefix).cloned())
}

/// Writes any accumulated character data as one text node.
fn flush(builder: &mut Builder, parent: usize, pending: &mut String) {
    if !pending.is_empty() {
        builder.text(parent, pending);
        pending.clear();
    }
}

/// Resolves one entity reference to the characters it stands for.
///
/// A reference this crate does not know — an HTML-only name like `&eacute;`, or a numeric one that
/// is not a valid character — is kept as written rather than dropped. Losing a character silently
/// would change an article's text and therefore its change-detection hash.
fn resolve_reference(reference: &quick_xml::events::BytesRef<'_>) -> String {
    if reference.is_char_ref() {
        if let Ok(Some(character)) = reference.resolve_char_ref() {
            return character.to_string();
        }
    }
    let name: &str = reference;
    crate::entities::xml_unescape(&format!("&{name};")).into_owned()
}

/// Splits a processing instruction's content into its target and the rest.
fn split_instruction(content: &str) -> (&str, &str) {
    match content.find(|c: char| c.is_ascii_whitespace()) {
        Some(at) => (&content[..at], content[at..].trim()),
        None => (content, ""),
    }
}

fn escape_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn escape_attribute(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#9;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluates an expression against the document root and renders the result the way XPath's
    /// `string()` would, so an assertion reads like the expression it tests.
    fn string(doc: &Doc, expression: &str) -> String {
        let expr = Doc::compile(expression).expect("a valid expression");
        let value = doc.evaluate(&expr, doc.root()).expect("evaluates");
        value.to_xpath_string()
    }

    /// The matched nodes, serialized.
    fn nodes(doc: &Doc, expression: &str) -> Vec<String> {
        let expr = Doc::compile(expression).expect("a valid expression");
        match doc.evaluate(&expr, doc.root()).expect("evaluates") {
            Value::NodeSet(found) => found.iter().map(|n| doc.serialize(*n)).collect(),
            other => vec![other.to_xpath_string()],
        }
    }

    const PAGE: &str = r#"<!doctype html>
<html><head><title>Café</title><base href="/sub/"></head>
<body>
	<div class="post"><h2>First</h2><p>One</p></div>
	<div class="post"><h2>Second</h2><p>Two<br>lines</p></div>
	<div class="ad"><h2>Skip me</h2></div>
	<!-- a comment -->
</body></html>"#;

    #[test]
    fn html_element_names_are_lowercased() {
        let doc = Doc::from_html("<HTML><BODY><DIV Class=\"X\">t</DIV></BODY></HTML>");
        assert_eq!(string(&doc, "count(//div)"), "1");
        assert_eq!(
            string(&doc, "//div/@class"),
            "X",
            "attribute values keep their case"
        );
    }

    #[test]
    fn html_is_parsed_leniently() {
        let doc = Doc::from_html("<div><p>Unclosed<p>Also<span>deep");
        assert_eq!(string(&doc, "count(//p)"), "2");
        assert_eq!(string(&doc, "//span"), "deep");
    }

    #[test]
    fn a_doctype_is_not_a_node() {
        let doc = Doc::from_html(PAGE);
        assert_eq!(
            string(&doc, "count(/node())"),
            "1",
            "just the <html> element"
        );
    }

    #[test]
    fn string_value_concatenates_descendant_text() {
        let doc = Doc::from_html(PAGE);
        // Whitespace *between* elements is text and survives; `normalize-space` collapses runs of it
        // but inserts nothing, so "FirstOne" is the correct answer for two adjacent elements.
        assert_eq!(
            string(&doc, "normalize-space(//body)"),
            "FirstOne SecondTwolines Skip me"
        );
        assert_eq!(
            string(&doc, "normalize-space(//div[1])"),
            "FirstOne",
            "a comment contributes nothing"
        );
    }

    #[test]
    fn predicates_and_axes_work() {
        let doc = Doc::from_html(PAGE);
        assert_eq!(string(&doc, "//div[@class='post']/h2"), "First");
        assert_eq!(string(&doc, "//div[@class='post'][2]/h2"), "Second");
        assert_eq!(string(&doc, "count(//div[@class='post'])"), "2");
        assert_eq!(string(&doc, "//div[last()]/h2"), "Skip me");
        assert_eq!(string(&doc, "//div[position()=2]/h2"), "Second");
        // The classic XPath trap, and one a scraper configuration hits often: `position()` counts
        // within each parent, so there is no h2 that is a second h2 child of its div.
        assert_eq!(string(&doc, "//h2[position()=2]"), "");
        assert_eq!(
            string(&doc, "(//h2)[2]"),
            "Second",
            "parentheses make it document-wide"
        );
        // A function call is not a node test, so `name()` wraps the path rather than following it.
        assert_eq!(string(&doc, "name(//p/following-sibling::*[1])"), "");
        assert_eq!(
            string(&doc, "name(//div[1]/h2/following-sibling::*[1])"),
            "p"
        );
    }

    #[test]
    fn a_relative_expression_uses_the_context_node() {
        let doc = Doc::from_html(PAGE);
        let items = Doc::compile("//div[@class='post']").unwrap();
        let Value::NodeSet(found) = doc.evaluate(&items, doc.root()).unwrap() else {
            panic!("expected a node set");
        };
        assert_eq!(found.len(), 2);
        let title = Doc::compile("normalize-space(./h2)").unwrap();
        let first = doc.evaluate(&title, found[0]).unwrap().to_xpath_string();
        let second = doc.evaluate(&title, found[1]).unwrap().to_xpath_string();
        assert_eq!((first.as_str(), second.as_str()), ("First", "Second"));
    }

    #[test]
    fn nodes_come_back_in_document_order() {
        let doc = Doc::from_html(PAGE);
        // `//h2 | //p` is a union, which XPath defines as document order regardless of the order the
        // operands are written in. Getting this wrong would scramble scraped articles.
        assert_eq!(
            nodes(&doc, "//h2 | //p"),
            vec![
                "<h2>First</h2>".to_owned(),
                "<p>One</p>".to_owned(),
                "<h2>Second</h2>".to_owned(),
                "<p>Two<br>lines</p>".to_owned(),
                "<h2>Skip me</h2>".to_owned(),
            ]
        );
    }

    #[test]
    fn serializing_html_writes_void_elements_without_a_close_tag() {
        let doc = Doc::from_html(r#"<p>a<br>b<img src="x.png">c</p>"#);
        assert_eq!(
            nodes(&doc, "//p"),
            vec![r#"<p>a<br>b<img src="x.png">c</p>"#.to_owned()]
        );
    }

    #[test]
    fn serializing_escapes_text_and_attributes() {
        let doc =
            Doc::from_html(r#"<p title="a &quot;b&quot; &amp; c">1 &lt; 2 &amp; 3 &gt; 2</p>"#);
        let out = nodes(&doc, "//p");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("1 &lt; 2 &amp; 3 &gt; 2"), "{}", out[0]);
        assert!(out[0].contains("&quot;b&quot;"), "{}", out[0]);
        assert!(out[0].contains("&amp; c"), "{}", out[0]);
    }

    #[test]
    fn the_base_href_is_resolved_against_the_document_url() {
        let doc = Doc::from_html(PAGE);
        assert_eq!(
            doc.base_href("https://e.example/page"),
            "https://e.example/sub/"
        );
        let without = Doc::from_html("<html><body>x</body></html>");
        assert_eq!(
            without.base_href("https://e.example/page"),
            "https://e.example/page"
        );
    }

    #[test]
    fn xml_preserves_case_and_is_strict() {
        let doc = Doc::from_xml("<Feed><Item id=\"7\"><Title>Hi</Title></Item></Feed>").unwrap();
        assert_eq!(string(&doc, "//Item/@id"), "7");
        assert_eq!(string(&doc, "//Item/Title"), "Hi");
        assert_eq!(string(&doc, "//item"), "", "XML names are case-sensitive");

        let err = Doc::from_xml("<Feed><Item></Feed>").unwrap_err();
        assert!(matches!(err, FeedError::Parse(_)), "{err}");

        // A document that simply stops is malformed too, and the reader has no notion of it, so the
        // builder checks: a truncated page must not scrape half its articles and report success.
        let err = Doc::from_xml("<Feed><Item>").unwrap_err();
        assert!(err.to_string().contains("left open"), "{err}");
    }

    #[test]
    fn xml_namespaces_let_a_prefixed_name_test_match() {
        let doc = Doc::from_xml(
            r#"<root xmlns:atom="http://www.w3.org/2005/Atom"><atom:entry><atom:title>T</atom:title></atom:entry></root>"#,
        )
        .unwrap();
        // Without a namespace hook the evaluator resolves `atom:` to nothing and silently matches
        // zero nodes, which is the worst possible failure mode for a scraped feed.
        assert_eq!(string(&doc, "count(//atom:entry)"), "1");
        assert_eq!(string(&doc, "//atom:entry/atom:title"), "T");
        assert_eq!(string(&doc, "count(//*[local-name()='entry'])"), "1");
    }

    #[test]
    fn a_default_namespace_does_not_apply_to_attributes() {
        let doc = Doc::from_xml(r#"<root xmlns="urn:x"><a href="/h">t</a></root>"#).unwrap();
        assert_eq!(string(&doc, "count(//*[local-name()='a'])"), "1");
        assert_eq!(string(&doc, "//*/@href"), "/h");
    }

    #[test]
    fn a_prefix_declared_on_a_descendant_is_in_scope_there() {
        let doc = Doc::from_xml(
            r#"<root><a xmlns:p="urn:one"><p:x>1</p:x></a><b xmlns:p="urn:two"><p:x>2</p:x></b></root>"#,
        )
        .unwrap();
        // Each element keeps the URI it was declared with…
        assert_eq!(string(&doc, "count(//*[local-name()='x'])"), "2");
        assert_eq!(
            string(
                &doc,
                "count(//*[local-name()='x'][namespace-uri()='urn:one'])"
            ),
            "1"
        );
        assert_eq!(
            string(
                &doc,
                "count(//*[local-name()='x'][namespace-uri()='urn:two'])"
            ),
            "1"
        );
        // …but a prefixed name test resolves through one flat prefix map, so it matches only the
        // first declaration of that prefix. `local-name()` is the reliable form, and the reason the
        // doc comment on `prefixes` says so out loud.
        assert_eq!(string(&doc, "count(//p:x)"), "1");
    }

    #[test]
    fn xml_entities_are_unescaped_and_cdata_is_literal() {
        let doc =
            Doc::from_xml(r#"<r><a>caf&amp;é &lt;tag&gt;</a><b><![CDATA[a < b & c]]></b></r>"#)
                .unwrap();
        assert_eq!(string(&doc, "//a"), "caf&é <tag>");
        assert_eq!(string(&doc, "//b"), "a < b & c");
    }

    #[test]
    fn serializing_xml_self_closes_empty_elements() {
        let doc = Doc::from_xml(r#"<r><a/><b>x</b></r>"#).unwrap();
        assert_eq!(nodes(&doc, "//r"), vec!["<r><a/><b>x</b></r>".to_owned()]);
    }

    #[test]
    fn comments_and_processing_instructions_survive_as_nodes() {
        let doc = Doc::from_xml(r#"<r><!--note--><?pi some data?><a/></r>"#).unwrap();
        assert_eq!(string(&doc, "//comment()"), "note");
        assert_eq!(string(&doc, "//processing-instruction()"), "some data");
        assert_eq!(string(&doc, "name(//processing-instruction())"), "pi");
    }

    #[test]
    fn a_bad_expression_is_a_configuration_error() {
        let err = Doc::compile("//div[@class").unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");

        // A function call is not a valid node test, so this one is rejected before evaluation.
        assert!(matches!(
            Doc::compile("//div/no-such-function()").unwrap_err(),
            FeedError::BadConfig(_)
        ));

        // An unknown function anywhere else is only discoverable at evaluation time.
        let doc = Doc::from_html(PAGE);
        let expr = Doc::compile("//div[nosuchfn()]").unwrap();
        let err = doc.evaluate(&expr, doc.root()).unwrap_err();
        assert!(matches!(err, FeedError::BadConfig(_)), "{err}");
        assert!(err.to_string().contains("nosuchfn()"), "{err}");
    }

    #[test]
    fn normalize_space_collapses_whitespace_like_xpath_says() {
        let doc = Doc::from_html("<p>  a\n\t b  </p>");
        assert_eq!(string(&doc, "normalize-space(//p)"), "a b");
        assert_eq!(string(&doc, "normalize-space('  x  ')"), "x");
    }

    #[test]
    fn a_deeply_nested_document_does_not_overflow_the_stack() {
        // 2 000 is far deeper than any real page and deep enough that a recursive walk of the tree
        // would overflow. The builders, the string-value walk and the serializer are all iterative.
        let depth = 2_000;
        let html = format!(
            "<div>{}x{}</div>",
            "<b>".repeat(depth),
            "</b>".repeat(depth)
        );
        let doc = Doc::from_html(&html);
        assert_eq!(string(&doc, "count(//b)"), depth.to_string());
        let expr = Doc::compile("//div").unwrap();
        let value = doc.evaluate(&expr, doc.root()).unwrap();
        let Value::NodeSet(found) = value else {
            panic!("expected a node set");
        };
        assert_eq!(found[0].string_value(), "x");
        assert!(!doc.serialize(found[0]).is_empty());
    }
}
