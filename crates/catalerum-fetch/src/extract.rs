//! CSS-selector HTML extraction (SOUL §27) — the precise companion to
//! [`html_to_markdown`](crate::markdown::html_to_markdown).
//!
//! Where the Markdown converter turns a whole page into cheap readable content,
//! this module pulls out *specific* parts of an HTML document by CSS selector:
//! the text of every `<h2>`, the `href` of each link in a list, the inner HTML of
//! a `<main>`. It is the building block for scraping a known field out of a fetched
//! page in an automation graph (e.g. `fetch_url` → `extract_html`).
//!
//! Like the converter it is **pure and deterministic** (no network, no JS): it
//! parses with `scraper` (html5ever) and reads matched elements. A malformed
//! selector is reported as an error; a selector that simply matches nothing yields
//! an empty result.

use scraper::{Html, Selector};

/// Which representation of a matched element [`extract_html`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtractField {
    /// The element's collapsed visible inner text (whitespace runs flattened).
    Text,
    /// The element's inner HTML markup (its children, not the element's own tag).
    InnerHtml,
    /// The element's full outer HTML markup (including its own tag).
    OuterHtml,
    /// The value of the named attribute. Matches lacking the attribute are skipped.
    Attr(String),
}

/// Extract a value from `html` for every element matching the CSS `selector`.
///
/// Returns the chosen [`ExtractField`] of each match in document order, capped at
/// `limit` matches when set. The result is `Err` **only** when the selector itself
/// is invalid — a well-formed selector that matches nothing yields an empty `Vec`.
/// For [`ExtractField::Attr`], elements that don't carry the attribute are skipped
/// rather than yielding an empty string.
///
/// # Errors
/// Returns a human-readable message when `selector` is not a valid CSS selector.
pub fn extract_html(
    html: &str,
    selector: &str,
    field: &ExtractField,
    limit: Option<usize>,
) -> Result<Vec<String>, String> {
    let sel =
        Selector::parse(selector).map_err(|e| format!("invalid CSS selector `{selector}`: {e}"))?;
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for el in doc.select(&sel) {
        if limit.is_some_and(|n| out.len() >= n) {
            break;
        }
        let value = match field {
            ExtractField::Text => collapse_ws(&el.text().collect::<String>()),
            ExtractField::InnerHtml => el.inner_html(),
            ExtractField::OuterHtml => el.html(),
            // Skip a match that lacks the attribute rather than emitting "".
            ExtractField::Attr(name) => match el.value().attr(name) {
                Some(v) => v.to_string(),
                None => continue,
            },
        };
        out.push(value);
    }
    Ok(out)
}

/// Collapse runs of ASCII/Unicode whitespace to single spaces and trim the ends —
/// so extracted text reads cleanly regardless of the source markup's indentation.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"
        <html><body>
          <article>
            <h1>Title</h1>
            <ul class="links">
              <li><a href="/a">First</a></li>
              <li><a href="/b">Second</a></li>
              <li><a>No href</a></li>
            </ul>
          </article>
        </body></html>
    "#;

    #[test]
    fn extracts_text_in_document_order() {
        let got = extract_html(DOC, "ul.links a", &ExtractField::Text, None).unwrap();
        assert_eq!(got, vec!["First", "Second", "No href"]);
    }

    #[test]
    fn extracts_attribute_skipping_missing() {
        let got =
            extract_html(DOC, "ul.links a", &ExtractField::Attr("href".into()), None).unwrap();
        // The third <a> has no href, so it's skipped rather than yielding "".
        assert_eq!(got, vec!["/a", "/b"]);
    }

    #[test]
    fn limit_caps_matches() {
        let got = extract_html(DOC, "li", &ExtractField::Text, Some(2)).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn collapses_text_whitespace() {
        let got =
            extract_html("<p>  hello\n   world  </p>", "p", &ExtractField::Text, None).unwrap();
        assert_eq!(got, vec!["hello world"]);
    }

    #[test]
    fn inner_and_outer_html_differ() {
        let inner =
            extract_html("<div><b>x</b></div>", "div", &ExtractField::InnerHtml, None).unwrap();
        let outer =
            extract_html("<div><b>x</b></div>", "div", &ExtractField::OuterHtml, None).unwrap();
        assert_eq!(inner, vec!["<b>x</b>"]);
        assert_eq!(outer, vec!["<div><b>x</b></div>"]);
    }

    #[test]
    fn no_match_is_empty_not_error() {
        let got = extract_html(DOC, "table", &ExtractField::Text, None).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn invalid_selector_is_error() {
        let err = extract_html(DOC, "a..b", &ExtractField::Text, None).unwrap_err();
        assert!(err.contains("invalid CSS selector"));
    }
}
