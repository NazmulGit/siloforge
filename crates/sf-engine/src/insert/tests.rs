use super::*;
use crate::html;

const URL: &str = "https://example.com/target";

fn req_for(raw: &str, idx: usize, anchor: &str) -> InsertRequest {
    let paras = html::paragraphs(raw, SplitMode::Prose).unwrap();
    InsertRequest {
        paragraph_index: idx,
        expected_hash: paras[idx].hash.clone(),
        anchor: anchor.into(),
        target_url: URL.into(),
        mode: SplitMode::Prose,
    }
}

fn ins(raw: &str, anchor: &str) -> Result<String, InsertError> {
    insert_link(raw, &req_for(raw, 0, anchor)).map(|o| o.content)
}

fn pad(s: &str) -> String {
    // keep paragraphs above the 40-char minimum
    format!("{s} Extra words keep this paragraph long enough.")
}

#[test]
fn golden_wraps_first_occurrence() {
    let raw = format!(
        "<p>{}</p>",
        pad("Learn about internal linking strategies today.")
    );
    let out = ins(&raw, "internal linking").unwrap();
    assert_eq!(
        out,
        format!(
            "<p>{}</p>",
            pad(&format!(
                "Learn about <a href=\"{URL}\">internal linking</a> strategies today."
            ))
        )
    );
}

#[test]
fn golden_case_insensitive_preserves_casing() {
    let raw = format!("<p>{}</p>", pad("Internal Linking is powerful."));
    assert!(
        ins(&raw, "internal linking")
            .unwrap()
            .contains(&format!("<a href=\"{URL}\">Internal Linking</a>"))
    );
}

#[test]
fn golden_anchor_nest_guard() {
    let raw = format!(
        "<p>{}</p>",
        pad("See <a href=\"/x\">internal linking</a> for more internal linking tips.")
    );
    let out = ins(&raw, "internal linking").unwrap();
    assert!(out.contains("<a href=\"/x\">internal linking</a>"));
    assert!(out.contains(&format!(
        "for more <a href=\"{URL}\">internal linking</a> tips."
    )));
    let only_linked = format!(
        "<p>{}</p>",
        pad("<a href=\"/x\">internal linking</a> rules.")
    );
    assert_eq!(
        ins(&only_linked, "internal linking"),
        Err(InsertError::AnchorNotFound)
    );
    let absent = format!("<p>{}</p>", pad("Nothing relevant here."));
    assert_eq!(
        ins(&absent, "internal linking"),
        Err(InsertError::AnchorNotFound)
    );
}

#[test]
fn golden_descends_into_inline_markup() {
    let raw = format!(
        "<p>{}</p>",
        pad("We cover <strong>great internal linking moves</strong>.")
    );
    assert!(ins(&raw, "internal linking").unwrap().contains(&format!(
        "<strong>great <a href=\"{URL}\">internal linking</a> moves</strong>"
    )));
}

#[test]
fn word_boundaries_and_tag_spanning() {
    let raw = format!("<p>{}</p>", pad("Bike locks and a bike lock."));
    let out = ins(&raw, "bike lock").unwrap();
    assert!(
        out.contains(&format!("a <a href=\"{URL}\">bike lock</a>.")),
        "{out}"
    );
    // cartoon must not match "cart"
    let raw2 = format!("<p>{}</p>", pad("A cartoon about shopping."));
    assert_eq!(ins(&raw2, "cart"), Err(InsertError::AnchorNotFound));
    // phrase spanning an inline element: wrap the whole <b>
    let raw3 = format!("<p>{}</p>", pad("The best <b>bike</b> locks are here."));
    let out3 = ins(&raw3, "best bike locks").unwrap();
    assert!(
        out3.contains(&format!("<a href=\"{URL}\">best <b>bike</b> locks</a>")),
        "{out3}"
    );
    // entity in text matches decoded anchor, source bytes preserved
    let raw4 = format!("<p>{}</p>", pad("Tips for R&amp;D teams today."));
    let out4 = ins(&raw4, "r&d teams").unwrap();
    assert!(
        out4.contains(&format!("<a href=\"{URL}\">R&amp;D teams</a>")),
        "{out4}"
    );
}

#[test]
fn never_inside_forbidden_places() {
    for inner in [
        "<code>bike lock</code>",
        "<button>bike lock</button>",
        "<kbd>bike lock</kbd>",
        "[caption id=\"1\"]bike lock[/caption]",
        "[shortcode attr=\"bike lock\"]",
    ] {
        let raw = format!(
            "<p>{}</p>",
            pad(&format!("Use {inner} carefully every day of the week."))
        );
        assert_eq!(
            ins(&raw, "bike lock"),
            Err(InsertError::AnchorNotFound),
            "{inner}"
        );
    }
    // a valid occurrence after a forbidden one is still found
    let raw = format!(
        "<p>{}</p>",
        pad("Use <code>bike lock</code> or a real bike lock.")
    );
    assert!(
        ins(&raw, "bike lock")
            .unwrap()
            .contains(&format!("real <a href=\"{URL}\">bike lock</a>"))
    );
}

#[test]
fn already_linked_and_bad_url() {
    let raw = format!(
        "<p>{}</p>",
        pad(&format!(
            "See <a href=\"{URL}/\">this</a> and the bike lock."
        ))
    );
    assert_eq!(ins(&raw, "bike lock"), Err(InsertError::AlreadyLinked));
    let raw2 = format!("<p>{}</p>", pad("The bike lock."));
    let mut r = req_for(&raw2, 0, "bike lock");
    for bad in [
        "javascript:alert(1)",
        "",
        "//evil.com/x",
        "data:text/html,x",
        "https://a b.com",
    ] {
        r.target_url = bad.into();
        assert_eq!(
            insert_link(&raw2, &r),
            Err(InsertError::InvalidUrl),
            "{bad}"
        );
    }
    r.target_url = "/relative/path?a=1&b=\"2\"".into();
    let out = insert_link(&raw2, &r).unwrap();
    assert!(
        out.content
            .contains("href=\"/relative/path?a=1&amp;b=&quot;2&quot;\"")
    );
    r.target_url = URL.into();
    r.anchor = "  ".into();
    assert_eq!(insert_link(&raw2, &r), Err(InsertError::EmptyAnchor));
}

#[test]
fn content_changed_is_typed() {
    let raw = format!(
        "<p>{}</p><p>{}</p>",
        pad("First paragraph about locks."),
        pad("Second about the bike lock.")
    );
    let mut r = req_for(&raw, 1, "bike lock");
    // Someone inserted a new paragraph at the top: index 1 now holds a different paragraph.
    let edited = format!("<p>{}</p>{raw}", pad("A brand new intro paragraph."));
    match insert_link(&edited, &r) {
        Err(InsertError::ContentChanged {
            index: 1,
            found_at: Some(2),
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
    r.paragraph_index = 9;
    assert!(matches!(
        insert_link(&raw, &r),
        Err(InsertError::ParagraphNotFound { index: 9, count: 2 })
    ));
    // locate_paragraph agrees
    assert!(
        locate_paragraph(
            &raw,
            0,
            &req_for(&raw, 0, "x").expected_hash,
            SplitMode::Prose
        )
        .is_ok()
    );
}

const GUTENBERG: &str = "<!-- wp:paragraph -->\n<p>Our guide covers the steel bike lock in depth, with tests.</p>\n<!-- /wp:paragraph -->\n\n<!-- wp:heading -->\n<h2>The steel bike lock</h2>\n<!-- /wp:heading -->\n\n<!-- wp:list -->\n<ul class=\"wp-block-list\"><li>A steel bike lock is heavy</li><li>Chains are heavier still</li></ul>\n<!-- /wp:list -->\n\n<!-- wp:shortcode -->\n[gallery ids=\"1,2\"]\n<!-- /wp:shortcode -->";

#[test]
fn gutenberg_round_trip_byte_for_byte() {
    for idx in 0..2 {
        let r = req_for(GUTENBERG, idx, "steel bike lock");
        let out = insert_link(GUTENBERG, &r).unwrap();
        assert!(
            out.content.contains("<!-- wp:paragraph -->")
                && out.content.contains("<!-- /wp:list -->")
        );
        assert!(!out.content.contains(&format!("<h2>The <a href=\"{URL}\">")));
        assert_eq!(out.content.matches("<a href=").count(), 1);
        assert_eq!(
            &out.content[out.link_start..out.link_end],
            format!("<a href=\"{URL}\">{}</a>", out.wrapped)
        );
        assert!(verify_link(&out.content, URL, "steel bike lock"));
        let undone =
            remove_link(&out.content, URL, "steel bike lock", Some(out.link_start)).unwrap();
        assert_eq!(undone, GUTENBERG, "idx {idx}");
    }
}

#[test]
fn classic_raw_round_trip_and_multiple_edges() {
    let raw = "First classic paragraph mentions the steel bike lock and more words here.\n\nSecond classic paragraph talks about a waterproof trunk bag for commuters.";
    let r1 = req_for(raw, 0, "steel bike lock");
    let a = insert_link(raw, &r1).unwrap();
    // Second edge on the same post, planned on the ORIGINAL content: paragraph 1 is untouched,
    // so its hash still matches after the first insertion.
    let mut r2 = req_for(raw, 1, "waterproof trunk bag");
    r2.target_url = "https://example.com/bags".into();
    let b = insert_link(&a.content, &r2).unwrap();
    assert_eq!(b.content.matches("<a href=").count(), 2);
    let u1 = remove_link(
        &b.content,
        "https://example.com/bags",
        "waterproof trunk bag",
        None,
    )
    .unwrap();
    let u2 = remove_link(&u1, URL, "steel bike lock", None).unwrap();
    assert_eq!(u2, raw);
    // Native links do not change canonical text, so the hash (and every index) stays valid
    // after an insertion; inserting the same edge twice is refused.
    assert_eq!(
        insert_link(&a.content, &r1),
        Err(InsertError::AlreadyLinked)
    );
}

#[test]
fn remove_link_picks_nearest_and_reports_missing() {
    let raw = format!(
        "<p>{}</p>",
        pad(&format!(
            "Old <a href=\"{URL}\">bike lock</a> link and a bike lock."
        ))
    );
    let mut r = req_for(&raw, 0, "bike lock");
    r.target_url = "https://example.com/other".into();
    let out = insert_link(&raw, &r).unwrap();
    let undone = remove_link(
        &out.content,
        "https://example.com/other",
        "bike lock",
        Some(out.link_start),
    )
    .unwrap();
    assert_eq!(undone, raw);
    assert_eq!(
        remove_link(&raw, "https://nope.com/", "x", None),
        Err(InsertError::LinkNotFound)
    );
}

#[test]
fn verify_counts_and_normalizes() {
    let html = format!("<div><a href=\"{URL}\">Bike\u{2019}s  Lock</a></div>");
    assert!(verify_link(&html, URL, "bike's lock"));
    assert!(verify_link(
        &html,
        "https://www.example.com/target/",
        "BIKE'S LOCK"
    ));
    let dup = format!("{html}{html}");
    assert!(!verify_link(&dup, URL, "bike's lock"));
    assert_eq!(count_links(&dup, URL, "bike's lock"), 2);
    assert!(!verify_link("<p>none</p>", URL, "x"));
}

#[test]
fn replace_and_append() {
    let raw = format!(
        "<!-- wp:paragraph -->\n<p>{}</p>\n<!-- /wp:paragraph -->",
        pad("Original text.")
    );
    let h = html::paragraphs(&raw, SplitMode::Prose).unwrap()[0]
        .hash
        .clone();
    let rep = replace_paragraph(&raw, 0, &h, "<p>New</p>", SplitMode::Prose).unwrap();
    assert_eq!(
        rep,
        "<!-- wp:paragraph -->\n<p>New</p>\n<!-- /wp:paragraph -->"
    );
    let app = append_to_paragraph(
        &raw,
        0,
        &h,
        "See <a href=\"/x\">this</a>.",
        SplitMode::Prose,
    )
    .unwrap();
    assert!(
        app.contains("enough. See <a href=\"/x\">this</a>.</p>\n<!-- /wp:paragraph -->"),
        "{app}"
    );
    assert!(matches!(
        replace_paragraph(&raw, 0, "deadbeef", "<p>x</p>", SplitMode::Prose),
        Err(InsertError::ContentChanged { .. })
    ));
}

#[test]
fn golden_pick_module() {
    use InsertModule::*;
    let all = [Native, Rephrase, Context];
    assert_eq!(pick_module(60.0, &all, None, true), Native);
    assert_eq!(pick_module(50.0, &all, None, true), Rephrase);
    assert_eq!(pick_module(35.0, &all, None, true), Context);
    assert_eq!(pick_module(25.0, &all, None, true), ContentGap);
    assert_eq!(pick_module(50.0, &[], None, true), ContentGap);
    assert_eq!(pick_module(70.0, &[Rephrase], None, true), Rephrase);
    assert_eq!(
        pick_module(65.0, &all, Some("conservative"), true),
        Rephrase
    );
    assert_eq!(pick_module(70.0, &all, Some("conservative"), true), Native);
    assert_eq!(pick_module(55.0, &all, Some("aggressive"), true), Native);
    assert_eq!(pick_module(21.0, &all, Some("aggressive"), true), Context);
    // B-17: anchors not in the paragraph cannot be native
    assert_eq!(pick_module(90.0, &all, None, false), Rephrase);
    assert_eq!(pick_module(f64::NAN, &all, None, true), ContentGap);
}

#[test]
fn hostile_input_never_panics() {
    let cases: Vec<String> = vec![
        String::new(),
        "<p".into(),
        "<a href=\"".into(),
        "<p>".repeat(30_000),
        "<b>".repeat(20_000) + "bike lock is a phrase long enough to be a paragraph here",
        "[a][/a][b x=1]".repeat(3000),
        "&#xFFFFFFFF;bike lock &amp".repeat(2000),
        format!("<p>{}</p>", "bike lock ".repeat(20_000)),
    ];
    for c in &cases {
        let paras = html::paragraphs(c, SplitMode::Prose).unwrap_or_default();
        let hash = paras.first().map(|p| p.hash.clone()).unwrap_or_default();
        let r = InsertRequest {
            paragraph_index: 0,
            expected_hash: hash.clone(),
            anchor: "bike lock".into(),
            target_url: URL.into(),
            mode: SplitMode::Prose,
        };
        let _ = insert_link(c, &r);
        let _ = remove_link(c, URL, "bike lock", Some(usize::MAX));
        let _ = verify_link(c, c, c);
        let _ = replace_paragraph(c, 0, &hash, c, SplitMode::V1);
        let _ = append_to_paragraph(c, 0, &hash, c, SplitMode::Prose);
        let _ = urls_match(c, c);
        let _ = validate_llm_output(LlmKind::Rephrase, c, c, c, c, &LlmLimits::default());
        let _ = validate_llm_output(LlmKind::Context, c, c, "", URL, &LlmLimits::default());
        let _ = rephrase_prompt(c, c, c, c);
        let _ = context_prompt(c, c, c, c);
        let _ = strip_code_fences(c);
    }
    let big = "x".repeat(html::MAX_HTML_BYTES + 1);
    assert_eq!(
        remove_link(&big, URL, "x", None),
        Err(InsertError::TooLarge)
    );
}
