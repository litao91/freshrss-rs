# fixtures

Offline samples that drive `cargo test`. Nothing here is fetched at test time — the whole corpus is
read from disk, and HTTP behaviour is exercised through
[`FixtureFetcher`](../crates/freshrss-feed/src/http/reqwest.rs) instead.

Every file is hand-written for this repository rather than copied from a live feed, so there are no
licensing or personal-data concerns and nothing here expires.

## Layout

```
feeds/<slug>/body.{xml,json}   a feed document, plus what it is meant to prove
hostile/                       the sanitizer corpus (see below)
```

Run any of them through the whole pipeline with the `parse` subcommand:

```
cargo run -- parse fixtures/feeds/rss2-basic/body.xml
cargo run -- parse fixtures/feeds/hostile/body.xml --show-content
cargo run -- parse fixtures/feeds/atom-basic/body.xml --unicity sha1:link_published --json
```

`parse` needs no database and no `config.toml`, so it is the fastest way to see what a parser change
did to a real document.

## What each feed covers

| Fixture | The behaviour it pins down |
|---|---|
| `rss2-basic` | The plain path: channel metadata, `atom:link rel=self/hub`, `<image>`, `dc:creator` preferred over `<author>`, `email (Name)` author parsing, comma-split categories |
| `rss2-no-guid` | The GUID fallback chain. No `<guid>` at all → `sha1(link.date)`; a permalink GUID doubling as the link; an *empty* `<guid></guid>` (supplied but blank, which is not the same as absent); two items sharing a GUID, which is what degrades the unicity policy |
| `rss2-content-encoded` | Body precedence: `content:encoded` over `description`, CDATA and escaped forms producing identical output, unescaped markup inside `<description>` keeping its tags, and a whitespace-only `content:encoded` falling through to the summary |
| `rss2-media` | Media RSS and podcast shapes: `<enclosure>` with an upper-case MIME type, `media:thumbnail`, `media:content` deriving its `medium` from the MIME type, `media:group` yielding exactly one rendition (the `isDefault` one), `atom:link rel=enclosure`, and non-remote `file://` / `data:` attachments being dropped |
| `rss2-bad-dates` | Every date spelling the parser accepts — named zones, missing weekday, single-digit day, doubled spaces, trailing `(UTC)` comment, `dc:date`, a space instead of `T`, bare unix seconds, epoch milliseconds, W3C `YYYY-MM` — plus the ones it must reject rather than guess at |
| `atom-basic` | Atom's differences from RSS: `xml:base` resolution of a relative `<link>`, `type="html"` titles, `published` winning over `updated` regardless of document order, `<summary>` as a fallback body, `type="text"` content being escaped rather than treated as markup, `rel=enclosure`, and a feed-level `<author>` *not* being inherited by entries |
| `atom-xhtml` | `type="xhtml"` wrapper `<div>` removal — including the case where two sibling divs are *not* a wrapper and must be left alone |
| `jsonfeed-basic` | JSON Feed 1.1: `content_html` over `content_text`, `summary` only as a last resort, `external_url` when there is no `url`, sub-second offsets, the 1.1 `authors` and 1.0 `author` forms, attachments deriving `medium` from `mime_type`, `icon` over `favicon`, an item with no `id`, and unknown fields being ignored |
| `rdf-basic` | RSS 1.0's structural difference — `<item>` elements are *siblings* of `<channel>`, not children — plus `rdf:about` supplying the GUID, and supplying the link only when it is actually remote |

## hostile/

The sanitizer corpus. Each item's title states what must happen to its body, and the whole file is
asserted against by `crates/freshrss-feed/src/sanitize/mod.rs`.

It covers: `<script>` and `<style>` with their bodies; every event-handler attribute; `javascript:`
in mixed case, with leading whitespace, with an embedded tab entity, and with an encoded first
letter; `vbscript:`, `file:`, `blob:`, `moz-extension:`; `data:` in every URL-bearing attribute
(only a non-SVG `data:image/…` on `img[src]` may survive); plugin and framing elements; `srcdoc`;
`style=` and `<style>`; `id`/`class` spoofing the reader's own `.rd-*` CSS and DOM ids; relative URL
resolution against the *feed's* base rather than the reader's origin; conditional comments; unbalanced
markup; deep nesting; and — equally important — the legitimate markup that must come through
untouched, including MathML.

To check it by hand:

```
cargo run -- parse fixtures/feeds/hostile/body.xml --show-content --json \
  | jq -r '.items[].content' \
  | grep -Ein 'script|on[a-z]+=|javascript:|vbscript:|data:image/svg|srcdoc|style='
```

That grep must produce no output.

## Adding a fixture

Add it for a *pathology*, not for popularity — a shape the parsers could plausibly get wrong. Then:

1. Put the body at `fixtures/feeds/<slug>/body.{xml,json}`.
2. Add a row to the table above saying what it pins down.
3. Add an assertion. A fixture nobody asserts against will drift into meaninglessness; either a test
   in the relevant parser module or a case in `crates/freshrss-feed/tests/`.
