use super::*;

const FIXTURE: &str = r#"
<h2>A Heading That Is Definitely Long Enough To Store</h2>
<p>First paragraph with enough characters to pass the forty character minimum easily.</p>
<p>too short</p>
<ul><li>List item one with some meaningful text</li><li>List item two with more meaningful text</li></ul>
<p>First paragraph with enough characters to pass the forty character minimum easily.</p>
<div><p>Nested paragraph inside a div wrapper that is long enough to count as content.</p></div>
<h3>Another Boundary Heading</h3>
<p>Final paragraph carrying plenty of characters so that it clears the minimum too.</p>
"#;

fn texts(html: &str, mode: SplitMode) -> Vec<String> {
    paragraphs(html, mode)
        .unwrap()
        .into_iter()
        .map(|p| p.text)
        .collect()
}

#[test]
fn golden_v1_chunk_fixture_both_modes() {
    let expected = vec![
        "First paragraph with enough characters to pass the forty character minimum easily.",
        "List item one with some meaningful text List item two with more meaningful text",
        "Nested paragraph inside a div wrapper that is long enough to count as content.",
        "Final paragraph carrying plenty of characters so that it clears the minimum too.",
    ];
    assert_eq!(texts(FIXTURE, SplitMode::V1), expected);
    assert_eq!(texts(FIXTURE, SplitMode::Prose), expected);
    assert!(texts("", SplitMode::V1).is_empty());
    assert!(texts("", SplitMode::Prose).is_empty());
}

#[test]
fn block_html_is_exact_source_slice() {
    let p = paragraphs(FIXTURE, SplitMode::Prose).unwrap();
    assert_eq!(
        p[0].html,
        "<p>First paragraph with enough characters to pass the forty character minimum easily.</p>"
    );
    for para in &p {
        assert_eq!(&FIXTURE[para.start..para.end], para.html);
        assert_eq!(para.hash, sha1_hex(&para.text));
        assert_eq!(para.hash.len(), 40);
    }
}

#[test]
fn golden_rust_engine_links() {
    let html = r#"
        <h2>A Heading That Is Definitely Long Enough To Store</h2>
        <p>First paragraph with enough characters to pass the forty character minimum easily.</p>
        <a href="http://external.example/path" rel="nofollow"> External link </a>
        <a href="/internal">Internal</a>
        <nav><a href="https://www.example.com/menu">Menu</a></nav>
    "#;
    let a = analyze(html, Some("https://www.example.com/post"), &[]).unwrap();
    assert_eq!(a.http_link_count, 1);
    assert_eq!(a.outbound_domains, vec!["external.example"]);
    assert_eq!(a.links[0].rel, vec!["nofollow"]);
    assert!(a.links[0].nofollow);
    assert_eq!(a.links[0].text, "External link");
    assert_eq!(
        a.links[1].resolved_url.as_deref(),
        Some("https://www.example.com/internal")
    );
    assert_eq!(a.links[1].is_external, Some(false));
    assert_eq!(a.links[2].placement, Placement::Nav);
    assert_eq!(a.links[0].placement, Placement::Body);
    assert_eq!(a.headings[0].level, 2);
}

#[test]
fn v1_hash_parity_known_value() {
    // sha1("abc") — guards the hex encoding.
    assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn gutenberg_raw_content_paragraphs() {
    let raw = "<!-- wp:paragraph -->\n<p>Gutenberg paragraph one has enough characters to be a real paragraph.</p>\n<!-- /wp:paragraph -->\n\n<!-- wp:heading -->\n<h2>Heading</h2>\n<!-- /wp:heading -->\n\n<!-- wp:list -->\n<ul><li>First list entry with text</li><li>Second list entry with text</li></ul>\n<!-- /wp:list -->";
    let p = paragraphs(raw, SplitMode::Prose).unwrap();
    assert_eq!(p.len(), 2);
    assert!(p[0].html.starts_with("<p>") && p[0].html.ends_with("</p>"));
    assert_eq!(detect_builder("", Some(raw)), Builder::Gutenberg);
}

#[test]
fn classic_raw_content_autop_runs() {
    let raw = "This is the first classic paragraph with <strong>bold</strong> text and enough length.\n\nSecond classic paragraph, also long enough to pass the minimum length check.\n\n[caption id=\"a\"]<img src=x> Caption text that is fairly long here, really[/caption]";
    let p = paragraphs(raw, SplitMode::Prose).unwrap();
    assert_eq!(p.len(), 3, "{p:#?}");
    assert_eq!(
        p[0].text,
        "This is the first classic paragraph with bold text and enough length."
    );
    assert_eq!(&raw[p[0].start..p[0].end], p[0].html);
    // shortcode tags stripped, content blocked
    assert_eq!(p[2].text, "Caption text that is fairly long here, really");
    assert_eq!(p[2].blocked, vec![(0, p[2].text.len())]);
    // v1 mode ignores bare top-level text
    assert_eq!(paragraphs(raw, SplitMode::V1).unwrap().len(), 0);
}

#[test]
fn prose_skips_non_prose_blocks() {
    let html = "<script>var long_variable_name = 'enough characters to pass forty';</script><pre>code block with more than forty characters in it ok</pre><p>A real paragraph of prose that is long enough to keep.</p>";
    assert_eq!(texts(html, SplitMode::Prose).len(), 1);
    assert_eq!(texts(html, SplitMode::V1).len(), 3);
}

#[test]
fn blocked_spans_cover_links_and_code() {
    let html = "<p>Read <a href=\"/x\">internal linking</a> and <code>bike lock</code> for the best bike lock today.</p>";
    let p = paragraphs(html, SplitMode::Prose).unwrap();
    let para = &p[0];
    assert_eq!(para.link_count, 1);
    let blocked: Vec<&str> = para
        .blocked
        .iter()
        .map(|&(s, e)| &para.text[s..e])
        .collect();
    assert_eq!(blocked, vec!["internal linking", "bike lock"]);
}

#[test]
fn whitespace_matches_js() {
    assert!(is_js_whitespace('\u{FEFF}'));
    assert!(is_js_whitespace('\u{A0}'));
    assert!(!is_js_whitespace('\u{85}'));
    assert_eq!(normalize_ws("  a\u{FEFF}\u{A0} b\n"), "a b");
    let html = "<p>Entities&nbsp;and&#160;BOM\u{FEFF}separate words in this paragraph text.</p>";
    assert_eq!(
        texts(html, SplitMode::V1),
        vec!["Entities and BOM separate words in this paragraph text."]
    );
}

#[test]
fn min_chars_counts_utf16_units() {
    // 20 astral chars = 40 UTF-16 units (kept); 39 BMP chars dropped.
    let astral = "\u{1F600}".repeat(20);
    assert_eq!(texts(&format!("<p>{astral}</p>"), SplitMode::V1).len(), 1);
    assert!(texts(&format!("<p>{}</p>", "a".repeat(39)), SplitMode::V1).is_empty());
}

#[test]
fn v1_list_duplicates_nested_li_prose_does_not() {
    let html = "<ul><li>Outer item text here<ul><li>Inner item text there</li></ul></li></ul>";
    assert_eq!(
        texts(html, SplitMode::V1),
        // cheerio `.text()` concatenates without a space between the outer text and the nested li
        vec!["Outer item text hereInner item text there Inner item text there"]
    );
    assert_eq!(
        texts(html, SplitMode::Prose),
        vec!["Outer item text here Inner item text there"]
    );
}

#[test]
fn builder_detection() {
    assert_eq!(
        detect_builder("<div class=\"elementor-widget x\">", None),
        Builder::Elementor
    );
    assert_eq!(detect_builder("[et_pb_section]", None), Builder::Divi);
    assert_eq!(
        detect_builder("<div class=\"a wp-block-group\">", None),
        Builder::Gutenberg
    );
    assert_eq!(detect_builder("<p>x</p>", None), Builder::Classic);
}

#[test]
fn html_to_text_and_word_count() {
    assert_eq!(html_to_text("<p>a</p><p>b<script>x()</script></p>"), "a b");
    assert_eq!(word_count(""), 0);
    assert_eq!(word_count(" one  two "), 2);
    assert_eq!(
        normalize_url("https://www.Example.com/a/b/?q=1"),
        "example.com/a/b"
    );
}

#[test]
fn base_href_honoured() {
    let a = analyze(
        "<base href=\"https://cdn.other.com/dir/\"><a href=\"page\">x</a>",
        Some("https://example.com/"),
        &[],
    )
    .unwrap();
    assert_eq!(
        a.links[0].resolved_url.as_deref(),
        Some("https://cdn.other.com/dir/page")
    );
    assert_eq!(a.links[0].is_external, Some(true));
}

#[test]
fn rejects_oversized() {
    let big = "x".repeat(MAX_HTML_BYTES + 1);
    assert!(matches!(
        analyze(&big, None, &[]),
        Err(HtmlError::TooLarge { .. })
    ));
    assert_eq!(html_to_text(&big), "");
}

#[test]
fn hostile_input_never_panics() {
    let mut cases: Vec<String> = vec![
        String::new(),
        "<".into(),
        "<<<>>>".into(),
        "<p".into(),
        "<a href=\"unterminated".into(),
        "</p></div></a>".into(),
        "<!--".into(),
        "<![CDATA[x".into(),
        "&#xFFFFFFFFFF;&#;&;&#x;&amp".into(),
        "<p>\u{0}\u{FFFF}\u{10FFFF}</p>".into(),
        "<script>".into(),
        "<scr<script>ipt>".into(),
        "[[x]] [/y] [a=".into(),
        "<a href=javascript:alert(1)>x</a>".into(),
        "<p>é漢字🙂</p>".into(),
    ];
    cases.push("<div>".repeat(5000));
    cases.push("<b><i>".repeat(3000) + "text long enough to be a paragraph of prose content");
    cases.push("<p>".repeat(20_000));
    cases.push("<ul>".to_string() + &"<li>item ".repeat(10_000));
    cases.push("&amp;".repeat(50_000));
    cases.push("[x a=1]".repeat(5_000));
    cases.push("a\n\n".repeat(10_000));
    for c in &cases {
        for mode in [SplitMode::Prose, SplitMode::V1] {
            let _ = analyze_with(c, Some("not a url"), &["ex.com".into()], mode);
        }
        let _ = html_to_text(c);
        let _ = detect_builder(c, Some(c));
        let _ = top_level_tags(c);
        let _ = word_count(c);
        let _ = normalize_url(c);
    }
}

/// Pilot 2026-10-13: comparison tables were one paragraph of glued cell text
/// ("Most Fun FeatureWheel Size: 12 inch...") and anchors were picked across cells. Tables are
/// skipped in Prose mode (V1 mode keeps v1 behaviour).
#[test]
fn prose_skips_tables() {
    let html = r#"<p>An opening paragraph that is easily long enough to be stored as prose.</p>
<table class="btie-style-short"><caption><h2>Our Top Bike Mirror Picks</h2></caption><tbody>
<tr><th>Product name heading cell that is long enough</th><td class="toc-tag">Most Fun Feature</td><td class="toc-pro1"><strong>Wheel Size</strong>: 12 inch</td></tr>
<tr><td>This review cell carries a full sentence of real prose about bike mirrors.</td></tr>
</tbody></table>"#;
    let t = texts(html, SplitMode::Prose);
    assert_eq!(
        t,
        vec!["An opening paragraph that is easily long enough to be stored as prose."]
    );
    assert!(
        texts(html, SplitMode::V1)
            .iter()
            .any(|x| x.contains("FeatureWheel"))
    );
}

/// Pilot 2026-10-13: "information Identify common bike" spanned two `<li>`; the text is unchanged
/// but the boundary is now a blocked span, so no anchor/phrase can cross it.
#[test]
fn prose_list_item_boundaries_are_blocked() {
    let html = "<ul><li>Update outdated information</li><li>Identify common bike problems and questions</li></ul>";
    let p = paragraphs(html, SplitMode::Prose).unwrap();
    let para = &p[0];
    assert_eq!(
        para.text,
        "Update outdated information Identify common bike problems and questions"
    );
    let boundary = para.text.find(" Identify").unwrap();
    assert!(
        para.blocked
            .iter()
            .any(|&(s, e)| s <= boundary && boundary < e),
        "{:?}",
        para.blocked
    );
    assert!(
        crate::anchor::find_unblocked(
            &para.text,
            "information Identify common bike",
            &para.blocked
        )
        .is_none()
    );
    assert!(
        crate::anchor::find_unblocked(&para.text, "common bike problems", &para.blocked).is_some()
    );
    // V1 mode is unchanged (no hard boundaries).
    let v1 = paragraphs(html, SplitMode::V1).unwrap();
    assert!(v1[0].blocked.is_empty());
}

/// Pilot 2026-10-13: review lists (`<ol><li><h2/><div/><p/></li>`) were one glued paragraph.
#[test]
fn prose_lists_of_sections_split_into_their_paragraphs() {
    let html = r#"<ol class="btie-style-reviews"><li><h2 id="x">Shimano PD-RS500 SPD-SL Road Pedal</h2><div class="btie-style-box"><a href="https://shop.example/x"><img src="a.jpg" alt=""></a><p>Best for Beginners</p><a href="https://shop.example/x">View Latest Price</a></div><p>If you are a recreational rider looking for a smooth entry into road cycling, this pedal is a fine pick.</p><p>The wide platform spreads pressure evenly so long rides stay comfortable for most riders.</p></li><li><h2>Second Pick</h2><p>Another review paragraph that is comfortably longer than forty characters.</p></li></ol>
<ul><li>Plain list item one with some meaningful text</li><li>Plain list item two with more meaningful text</li></ul>"#;
    let t = texts(html, SplitMode::Prose);
    assert_eq!(
        t,
        vec![
            "If you are a recreational rider looking for a smooth entry into road cycling, this pedal is a fine pick.",
            "The wide platform spreads pressure evenly so long rides stay comfortable for most riders.",
            "Another review paragraph that is comfortably longer than forty characters.",
            "Plain list item one with some meaningful text Plain list item two with more meaningful text",
        ]
    );
    // every block's html is still an exact source slice (insertion relies on it)
    for p in paragraphs(html, SplitMode::Prose).unwrap() {
        assert_eq!(html.get(p.start..p.end), Some(p.html.as_str()));
    }
}
