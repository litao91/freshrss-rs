//! The element and attribute whitelist.
//!
//! Transcribed from `FreshRSS_SimplePieCustom::__construct()` (`app/Models/SimplePieCustom.php`),
//! which configures SimplePie's sanitizer. The list is long and looks arbitrary in places — `marquee`,
//! `blink`, `xmp` — but that is the point: feeds in the wild use them, and a sanitizer that drops
//! legitimate markup is one users disable.
//!
//! Every entry is opt-in. Adding a tag or attribute here is a security decision and should be
//! reviewed as one.

/// Elements allowed to survive.
///
/// Notably absent, and deliberately so:
/// - `script`, `style`, `object`, `embed`, `applet`, `form`, `input`, `link`, `meta` — see
///   [`CLEAN_CONTENT_TAGS`] for which of these have their *bodies* removed too.
/// - `area`, `map` — FreshRSS leaves them out until in-article `#fragment` ids can be rewritten
///   safely; an `<area>` pointing at `#id` would otherwise escape the article.
/// - `noscript`, `noembed` — their contents are wanted, and ammonia keeps the children of an
///   unlisted tag, which is exactly right.
/// - `plaintext` — it cannot be closed, so it would swallow the rest of the document.
/// - SVG — `xlink:href` on SVG animation elements is a script-injection vector that needs its own
///   URL rewriting pass.
pub const TAGS: &[&str] = &[
    // HTML
    "a", "abbr", "acronym", "address", "article", "aside", "audio", "b", "bdi", "bdo", "big",
    "blink", "blockquote", "br", "button", "canvas", "caption", "center", "cite", "code", "col",
    "colgroup", "data", "datalist", "dd", "del", "details", "dfn", "dialog", "dir", "div", "dl",
    "dt", "em", "fieldset", "figcaption", "figure", "footer", "h1", "h2", "h3", "h4", "h5", "h6",
    "header", "hgroup", "hr", "i", "iframe", "image", "img", "ins", "kbd", "label", "legend", "li",
    "main", "mark", "marquee", "menu", "meter", "nav", "nobr", "noframes", "ol", "optgroup",
    "option", "output", "p", "picture", "pre", "progress", "q", "rb", "rp", "rt", "rtc", "ruby",
    "s", "samp", "search", "section", "select", "small", "source", "span", "strike", "strong",
    "sub", "summary", "sup", "table", "tbody", "td", "textarea", "tfoot", "th", "thead", "time",
    "tr", "track", "tt", "u", "ul", "var", "video", "wbr", "xmp",
    // MathML
    "maction", "math", "menclose", "merror", "mfenced", "mfrac", "mi", "mmultiscripts", "mn", "mo",
    "mover", "mpadded", "mphantom", "mprescripts", "mroot", "mrow", "ms", "mspace", "msqrt", "msub",
    "msubsup", "msup", "mtable", "mtd", "mtext", "mtr", "munder", "munderover",
];

/// Attributes allowed on any element in [`TAGS`].
pub const GENERIC_ATTRIBUTES: &[&str] = &[
    // HTML
    "dir", "draggable", "hidden", "lang", "role", "title",
    // MathML
    "displaystyle", "mathsize", "scriptlevel",
];

/// `aria-*` and `data-*` are allowed by prefix, matching FreshRSS's `allow_aria_attr(true)` and
/// `allow_data_attr(true)`.
pub const GENERIC_ATTRIBUTE_PREFIXES: &[&str] = &["aria-", "data-"];

/// `data-*` attributes that are stripped even though the prefix is allowed.
///
/// These are SimplePie's own bookkeeping keys. Left in place they would let one feed's content
/// influence how another pass re-sanitizes it.
pub const STRIPPED_DATA_ATTRIBUTES: &[&str] = &[
    "data-auto-leave-validation",
    "data-leave-validation",
    "data-no-leave-validation",
    "data-original",
];

/// Elements whose *content* is removed along with the tag.
///
/// ammonia's default is `{script, style}`, and nothing here needs adding: `xmp` and `noframes` are
/// allowed elements whose content is meant to be shown.
pub const CLEAN_CONTENT_TAGS: &[&str] = &["script", "style"];

/// Per-element attribute whitelist, as `(element, attributes)`.
///
/// Kept as a slice of slices rather than a `HashMap` so the table reads like the PHP it came from
/// and stays in alphabetical order for diffing.
pub const TAG_ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href", "hreflang", "type"]),
    ("audio", &["controlslist", "loop", "muted", "src"]),
    ("blockquote", &["cite"]),
    ("br", &["clear"]),
    ("button", &["disabled"]),
    ("canvas", &["width", "height"]),
    ("caption", &["align"]),
    ("col", &["span", "align", "valign", "width"]),
    ("colgroup", &["span", "align", "valign", "width"]),
    ("data", &["value"]),
    ("del", &["cite", "datetime"]),
    ("details", &["open"]),
    ("div", &["align"]),
    ("fieldset", &["disabled"]),
    ("hr", &["align", "noshade", "size", "width"]),
    (
        "iframe",
        &[
            "src",
            "align",
            "frameborder",
            "longdesc",
            "marginheight",
            "marginwidth",
            "scrolling",
            "allowfullscreen",
        ],
    ),
    (
        "image",
        &[
            "src",
            "alt",
            "width",
            "height",
            "align",
            "border",
            "hspace",
            "longdesc",
            "vspace",
        ],
    ),
    (
        "img",
        &[
            "src",
            "alt",
            "width",
            "height",
            "align",
            "border",
            "hspace",
            "longdesc",
            "vspace",
        ],
    ),
    ("ins", &["cite", "datetime"]),
    ("li", &["value", "type"]),
    (
        "marquee",
        &[
            "behavior",
            "direction",
            "height",
            "hspace",
            "loop",
            "scrollamount",
            "scrolldelay",
            "truespeed",
            "vspace",
            "width",
        ],
    ),
    ("meter", &["value", "min", "max", "low", "high", "optimum"]),
    ("ol", &["reversed", "start", "type"]),
    ("optgroup", &["disabled", "label"]),
    ("option", &["disabled", "label", "selected", "value"]),
    ("p", &["align"]),
    ("pre", &["width", "wrap"]),
    ("progress", &["max", "value"]),
    ("q", &["cite"]),
    ("select", &["disabled", "multiple", "size"]),
    ("source", &["type", "src", "media", "height", "width"]),
    (
        "table",
        &[
            "align",
            "border",
            "cellpadding",
            "cellspacing",
            "rules",
            "summary",
            "width",
        ],
    ),
    ("tbody", &["align", "char", "charoff", "valign"]),
    (
        "td",
        &[
            "colspan",
            "headers",
            "rowspan",
            "abbr",
            "align",
            "height",
            "scope",
            "valign",
            "width",
        ],
    ),
    (
        "textarea",
        &[
            "cols",
            "disabled",
            "maxlength",
            "minlength",
            "placeholder",
            "readonly",
            "rows",
            "wrap",
        ],
    ),
    ("tfoot", &["align", "valign"]),
    (
        "th",
        &[
            "abbr",
            "colspan",
            "rowspan",
            "scope",
            "align",
            "height",
            "valign",
            "width",
        ],
    ),
    ("thead", &["align", "valign"]),
    ("time", &["datetime"]),
    ("tr", &["align", "valign"]),
    ("track", &["default", "kind", "srclang", "label", "src"]),
    ("ul", &["type"]),
    (
        "video",
        &[
            "src",
            "poster",
            "controlslist",
            "height",
            "loop",
            "muted",
            "playsinline",
            "width",
        ],
    ),
    // MathML
    ("maction", &["actiontype", "selection"]),
    ("math", &["display"]),
    ("menclose", &["notation"]),
    ("mfenced", &["close", "open", "separators"]),
    ("mfrac", &["denomalign", "linethickness", "numalign"]),
    ("mi", &["mathvariant"]),
    ("mmultiscripts", &["subscriptshift", "superscriptshift"]),
    (
        "mo",
        &[
            "accent",
            "fence",
            "form",
            "largeop",
            "lspace",
            "maxsize",
            "minsize",
            "movablelimits",
            "rspace",
            "separator",
            "stretchy",
            "symmetric",
        ],
    ),
    ("mover", &["accent"]),
    ("mpadded", &["depth", "height", "lspace", "voffset", "width"]),
    ("mspace", &["depth", "height", "width"]),
    ("msubsup", &["subscriptshift", "superscriptshift"]),
    ("msup", &["superscriptshift"]),
    (
        "mtable",
        &[
            "align",
            "columnalign",
            "columnlines",
            "columnspacing",
            "frame",
            "framespacing",
            "rowalign",
            "rowlines",
            "rowspacing",
            "width",
        ],
    ),
    ("mtd", &["columnspan", "rowspan", "columnalign", "rowalign"]),
    ("mtr", &["columnalign", "rowalign"]),
    ("munder", &["accentunder"]),
    ("munderover", &["accent", "accentunder"]),
];

/// Attributes forced onto an element, matching FreshRSS's `add_attributes`.
///
/// `preload="none"` on `audio`/`video` matters more than it looks: without it, a feed with twenty
/// podcast episodes makes the reader open twenty connections the moment an article is displayed.
pub const ADDED_ATTRIBUTES: &[(&str, &str, &str)] = &[
    ("audio", "controls", "controls"),
    ("audio", "preload", "none"),
    ("video", "controls", "controls"),
    ("video", "preload", "none"),
    (
        "iframe",
        "allow",
        "accelerometer; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share",
    ),
    ("iframe", "sandbox", IFRAME_SANDBOX),
    ("iframe", "allowfullscreen", "allowfullscreen"),
];

/// The `sandbox` value forced onto every `<iframe>`.
///
/// `allow-same-origin` alongside `allow-scripts` is not a sandbox against a *same-origin* frame:
/// such a document could script its way out. It is kept because embeds users actually want
/// (YouTube, Vimeo, Bandcamp) break without it, and the frame's origin is the remote site's, not
/// ours. The remaining protection is that [`crate::sanitize::uri`] refuses a non-`http(s)` `src`,
/// so a frame can never be pointed at our own origin by a relative URL.
pub const IFRAME_SANDBOX: &str = "allow-scripts allow-same-origin";

/// Attributes that carry a URL and so go through the scheme check, as `(element, attribute)`.
///
/// ammonia checks `href`, `src`, `poster` and a few others itself. `cite` and `longdesc` are *not*
/// in its list, but FreshRSS treats them as URLs (its `set_url_replacements` includes them), so they
/// are validated here too — an unvalidated `cite` is harmless today but is exactly the kind of gap
/// that becomes a bug when a browser starts honouring it.
pub const URL_ATTRIBUTES: &[(&str, &str)] = &[
    ("a", "href"),
    ("audio", "src"),
    ("blockquote", "cite"),
    ("del", "cite"),
    ("iframe", "src"),
    ("image", "longdesc"),
    ("image", "src"),
    ("img", "longdesc"),
    ("img", "src"),
    ("ins", "cite"),
    ("q", "cite"),
    ("source", "src"),
    ("track", "src"),
    ("video", "poster"),
    ("video", "src"),
];

/// True when this `(element, attribute)` pair holds a URL.
pub fn is_url_attribute(element: &str, attribute: &str) -> bool {
    URL_ATTRIBUTES
        .iter()
        .any(|(e, a)| *e == element && *a == attribute)
        // ammonia's own set, so the two never disagree about the common cases.
        || matches!(attribute, "href" | "src" | "xlink:href")
}

/// The `rel` forced onto every `<a>`.
///
/// `noopener` stops the opened page from reaching `window.opener` and navigating the reader away;
/// `noreferrer` keeps the article's URL out of the target's logs.
pub const LINK_REL: &str = "noopener noreferrer";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_elements_are_not_allowed() {
        for tag in [
            "script", "style", "object", "embed", "applet", "form", "input", "link", "meta",
            "base", "svg", "animate", "plaintext", "area", "map", "noscript", "noembed", "frame",
            "frameset", "body", "html", "head",
        ] {
            assert!(!TAGS.contains(&tag), "<{tag}> must not be in the allowlist");
        }
    }

    #[test]
    fn script_and_style_lose_their_bodies_too() {
        assert!(CLEAN_CONTENT_TAGS.contains(&"script"));
        assert!(CLEAN_CONTENT_TAGS.contains(&"style"));
    }

    #[test]
    fn no_dangerous_attribute_is_allowed_anywhere() {
        let banned = [
            "style", "onclick", "onerror", "onload", "formaction", "srcdoc", "background",
            "dynsrc", "lowsrc", "ping", "codebase", "classid", "data", "id", "class",
        ];
        for attr in banned {
            assert!(
                !GENERIC_ATTRIBUTES.contains(&attr),
                "{attr} must not be a generic attribute"
            );
            for (tag, list) in TAG_ATTRIBUTES {
                assert!(!list.contains(&attr), "{attr} must not be allowed on <{tag}>");
            }
        }
    }

    #[test]
    fn every_tag_with_attributes_is_an_allowed_tag() {
        for (tag, _) in TAG_ATTRIBUTES {
            assert!(TAGS.contains(tag), "<{tag}> has attributes but is not allowed");
        }
    }

    #[test]
    fn every_forced_attribute_lands_on_its_tag() {
        // Asserted behaviourally rather than by table lookup: ammonia's `set_tag_attribute_values`
        // adds these *after* the allowlist is applied, so `controls` and `preload` do not need to be
        // listed on `<audio>`/`<video>` to survive. What matters is that they do survive.
        for (tag, attr, value) in ADDED_ATTRIBUTES {
            assert!(TAGS.contains(tag), "<{tag}> is not an allowed element");
            let out = crate::sanitize::sanitize_html(
                &format!("<{tag}></{tag}>"),
                &crate::sanitize::SanitizeOptions::default(),
            );
            assert!(
                out.contains(&format!("{attr}=\"{value}\""))
                    || out.contains(&format!("{attr}={value}")),
                "<{tag}> came back without {attr}: {out}"
            );
        }
    }

    #[test]
    fn the_tag_table_is_alphabetical_and_free_of_duplicates() {
        let mut sorted = TAGS.to_vec();
        sorted.sort_unstable();
        // MathML follows the HTML block, so only the HTML prefix is expected to be sorted.
        let html_end = TAGS.iter().position(|t| *t == "maction").unwrap_or(TAGS.len());
        let mut html: Vec<&str> = TAGS[..html_end].to_vec();
        let original = html.clone();
        html.sort_unstable();
        assert_eq!(html, original, "the HTML block should stay alphabetical");

        let mut all = TAGS.to_vec();
        let before = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), before, "no duplicate tags");
        let _ = sorted;
    }

    #[test]
    fn the_attribute_table_has_no_duplicate_entries() {
        for (tag, list) in TAG_ATTRIBUTES {
            let mut v = list.to_vec();
            let before = v.len();
            v.sort_unstable();
            v.dedup();
            assert_eq!(v.len(), before, "<{tag}> lists an attribute twice");
        }
    }

    #[test]
    fn url_attributes_are_recognized() {
        assert!(is_url_attribute("img", "src"));
        assert!(is_url_attribute("blockquote", "cite"));
        assert!(is_url_attribute("img", "longdesc"));
        assert!(is_url_attribute("video", "poster"));
        assert!(!is_url_attribute("img", "alt"));
        assert!(!is_url_attribute("p", "align"));
        // ammonia's generic rule catches anything we forgot to list.
        assert!(is_url_attribute("whatever", "href"));
    }

    #[test]
    fn iframes_are_sandboxed_and_media_does_not_autoplay() {
        let added = |tag: &str, attr: &str| {
            ADDED_ATTRIBUTES
                .iter()
                .find(|(t, a, _)| *t == tag && *a == attr)
                .map(|(_, _, v)| *v)
        };
        assert!(added("iframe", "sandbox").is_some_and(|v| v.contains("allow-scripts")));
        assert_eq!(added("audio", "preload"), Some("none"));
        assert_eq!(added("video", "preload"), Some("none"));
        assert_eq!(added("audio", "controls"), Some("controls"));
    }
}
