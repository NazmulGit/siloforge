use super::*;

const U1: &str = "11111111-1111-1111-1111-111111111111";
const S1: &str = "22222222-2222-2222-2222-222222222222";
const S2: &str = "22222222-2222-2222-2222-333333333333";
const C1: &str = "33333333-3333-3333-3333-333333333333";
const P1: &str = "44444444-4444-4444-4444-444444444444";
const P2: &str = "44444444-4444-4444-4444-555555555555";
const P3: &str = "44444444-4444-4444-4444-666666666666";
const G1: &str = "55555555-5555-5555-5555-555555555551";
const G2: &str = "55555555-5555-5555-5555-555555555552";
const G3: &str = "55555555-5555-5555-5555-555555555553";

fn vec_text(seed: f32) -> String {
    let v: Vec<String> = (0..EMBED_DIM)
        .map(|i| format!("{}", ((i as f32 + seed) * 0.37).sin() * 0.1))
        .collect();
    format!("[{}]", v.join(","))
}

/// Writes CSVs shaped like v1 `COPY ... TO STDOUT WITH (FORMAT csv, HEADER, NULL '\N')` output.
fn fixture_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sf-ops-mig-{}", sf_core::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let w =
        |name: &str, body: String| std::fs::write(dir.join(format!("{name}.csv")), body).unwrap();
    w(
        "users",
        format!(
            "id,email,password_hash,name,role,created_at\n\
             {U1},admin@x.io,$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy,\"Admin, Big\",admin,\\N\n"
        ),
    );
    w(
        "sites",
        format!(
            "id,domain,wp_url,mode,api_key,settings,created_at,last_sync_at,user_id\n\
             {S1},a.com,https://a.com,wp_rest,sf_aaa,\"{{\"\"wp_user\"\":\"\"bob\"\",\"\"wp_app_password\"\":\"\"abcd efgh\"\",\"\"insert_modules\"\":[\"\"native\"\"]}}\",2026-09-01 10:00:00.123456+00,\\N,{U1}\n\
             {S2},b.com,,wp_rest,sf_bbb,{{}},2026-09-02 10:00:00+02,2026-09-03 01:02:03.5+00,\\N\n"
        ),
    );
    w(
        "clusters",
        format!(
            "id,site_id,label,pillar_post_id,created_at,source\n\
             {C1},{S1},Bottles,{P1},2026-09-01 10:00:00+00,auto\n"
        ),
    );
    w(
        "posts",
        format!(
            "id,site_id,wp_post_id,url,slug,title,content_html,content_text,content_hash,builder,post_type,post_status,meta,word_count,cluster_id,pagerank,click_depth,is_orphan,is_dead_end,embedding,published_at,modified_at,crawled_at,is_contextual_orphan,legacy_extra\n\
             {P1},{S1},101,https://a.com/p1,p1,P1,\"<p>Hi, \"\"there\"\"</p>\",Hi there,h1,gutenberg,post,publish,{{}},2,{C1},0.25,1,t,f,\"{}\",2026-09-01 00:00:00+00,\\N,2026-09-01 10:00:00+00,f,zzz\n\
             {P2},{S1},\\N,https://a.com/p2,\\N,P2,,,h2,gutenberg,page,publish,\"{{\"\"k\"\":1}}\",0,\\N,0,-1,f,t,\\N,\\N,\\N,2026-09-01 10:00:00+00,t,zzz\n\
             {P3},{S2},7,https://b.com/p3,p3,P3,x,x,h3,classic,post,draft,not-json,1,\\N,0,0,f,f,\"[1,2,3]\",\\N,\\N,2026-09-02 10:00:00+00,f,zzz\n",
            vec_text(1.0)
        ),
    );
    w(
        "paragraphs",
        format!(
            "id,post_id,idx,text,hash,embedding\n\
             {G1},{P1},0,First para,ph1,\"{}\"\n\
             {G2},{P1},1,\"Second, with \"\"quotes\"\"\",ph2,\\N\n\
             {G3},{P3},0,Other site,ph3,\"{}\"\n",
            vec_text(2.0),
            vec_text(3.0)
        ),
    );
    w(
        "keywords",
        format!(
            "id,post_id,keyword,source,score,volume,position,striking\n\
             66666666-6666-6666-6666-666666666661,{P1},bottle,title,1.5,\\N,3.2,t\n\
             66666666-6666-6666-6666-666666666662,{P1},cap,body,0.5,10,\\N,f\n"
        ),
    );
    w(
        "job_runs",
        format!(
            "id,site_id,kind,status,stats,error,started_at,finished_at,created_at\n\
             77777777-7777-7777-7777-777777777771,{S1},crawl,done,\"{{\"\"upserted\"\":3}}\",\\N,2026-09-01 10:00:00+00,2026-09-01 10:01:00+00,2026-09-01 10:00:00+00\n\
             77777777-7777-7777-7777-777777777772,{S1},plan,running,{{}},\\N,2026-09-01 10:00:00+00,\\N,2026-09-01 10:00:00+00\n\
             77777777-7777-7777-7777-777777777773,\\N,maintenance,done,{{}},\\N,\\N,\\N,2026-09-01 10:00:00+00\n"
        ),
    );
    dir
}

fn open_db(dir: &std::path::Path) -> Db {
    Db::open(&dir.join("v2").join("siloforge.db")).unwrap()
}

fn key() -> [u8; 32] {
    [9u8; 32]
}

#[test]
fn convert_cells() {
    let mut bad = false;
    assert_eq!(convert("\\N", Ty::NText, &mut bad), SqlValue::Null);
    assert_eq!(
        convert("", Ty::Text, &mut bad),
        SqlValue::Text(String::new())
    );
    assert_eq!(convert("", Ty::NInt, &mut bad), SqlValue::Null);
    assert_eq!(convert("\\N", Ty::Int, &mut bad), SqlValue::Integer(0));
    assert_eq!(convert("t", Ty::Bool, &mut bad), SqlValue::Integer(1));
    assert_eq!(convert("f", Ty::Bool, &mut bad), SqlValue::Integer(0));
    assert_eq!(convert("3.0", Ty::Int, &mut bad), SqlValue::Integer(3));
    assert_eq!(convert("nan", Ty::Real, &mut bad), SqlValue::Real(0.0));
    assert_eq!(
        convert("oops", Ty::JsonObj, &mut bad),
        SqlValue::Text("{}".into())
    );
    assert_eq!(
        convert("\\N", Ty::JsonArr, &mut bad),
        SqlValue::Text("[]".into())
    );
    assert!(!bad);
    assert_eq!(convert("[1,2]", Ty::Vector, &mut bad), SqlValue::Null);
    assert!(bad);
    assert_eq!(
        pg_ts_to_rfc3339("2026-10-04 12:34:56.123456+00").as_deref(),
        Some("2026-10-04T12:34:56.123Z")
    );
    assert_eq!(
        pg_ts_to_rfc3339("2026-10-04 14:34:56+02").as_deref(),
        Some("2026-10-04T12:34:56.000Z")
    );
    assert_eq!(
        pg_ts_to_rfc3339("2026-10-04 12:34:56+05:30").as_deref(),
        Some("2026-10-04T07:04:56.000Z")
    );
    assert_eq!(
        pg_ts_to_rfc3339("2026-10-04T12:34:56Z").as_deref(),
        Some("2026-10-04T12:34:56.000Z")
    );
    assert_eq!(
        pg_ts_to_rfc3339("2026-10-04").as_deref(),
        Some("2026-10-04T00:00:00.000Z")
    );
    assert_eq!(pg_ts_to_rfc3339("garbage"), None);
    // hostile input never panics
    for s in [
        "é+0",
        "+0",
        "ü",
        "\u{0}",
        "-99",
        "[",
        "[,]",
        "2026-13-45 99:99:99+99",
    ] {
        for ty in [Ty::Ts, Ty::Int, Ty::Real, Ty::Bool, Ty::JsonObj, Ty::Vector] {
            let _ = convert(s, ty, &mut bad);
        }
        let _ = pg_ts_to_rfc3339(s);
    }
    assert!(is_uuid(U1) && !is_uuid("x'; DROP TABLE x; --"));
}

#[test]
fn import_from_csv_dir_maps_everything() {
    let dir = fixture_dir();
    let db = open_db(&dir);
    let seen = std::sync::Mutex::new(Vec::new());
    let rep = import_from_dir(&dir, &db, &MigrateOptions::new(key()), &|p| {
        seen.lock().unwrap().push(p.clone())
    })
    .unwrap();
    let t = |n: &str| rep.tables.iter().find(|t| t.table == n).unwrap().clone();
    assert_eq!(t("posts").inserted, 3);
    assert_eq!(t("posts").invalid_vectors, 1); // [1,2,3] is not 384-d
    assert_eq!(t("paragraphs").inserted, 3);
    assert!(!t("snapshots").present);
    assert_eq!(rep.pillars_linked, 1);
    assert_eq!(rep.secrets_moved, 1);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|p| p.table == "paragraphs" && p.done)
    );
    let text = rep.to_string();
    assert!(text.contains("posts") && text.contains("not in source"));

    db.read_blocking(|c| {
        // users: bcrypt kept, NULL created_at defaulted
        let (h, name, created): (String, String, String) = c
            .query_row("SELECT password_hash, name, created_at FROM users", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert!(h.starts_with("$2a$10$") && name == "Admin, Big" && created.ends_with('Z'));
        // sites: secrets moved + encrypted, timestamps normalised, empty wp_url -> NULL
        let (settings, enc, created): (String, String, String) = c
            .query_row(
                "SELECT settings, secrets_enc, created_at FROM sites WHERE id=?1",
                [S1],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(!settings.contains("abcd") && settings.contains("insert_modules"));
        assert!(!enc.is_empty() && !enc.contains("abcd"));
        let sec = SiteSecrets::load(&key(), &enc).unwrap();
        assert_eq!(sec.wp_app_password.as_deref(), Some("abcd efgh"));
        assert_eq!(created, "2026-09-01T10:00:00.123Z");
        let (wp, created2, enc2): (Option<String>, String, String) = c
            .query_row(
                "SELECT wp_url, created_at, secrets_enc FROM sites WHERE id=?1",
                [S2],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(wp, None);
        assert_eq!(created2, "2026-09-02T08:00:00.000Z");
        assert_eq!(enc2, "");
        // posts: chunked_hash, booleans, vectors, quoted html, invalid json -> {}
        let (ch, orphan, dead, emb, html, cluster): (String, i64, i64, Option<Vec<u8>>, String, Option<String>) = c
            .query_row(
                "SELECT chunked_hash, is_orphan, is_dead_end, embedding, content_html, cluster_id FROM posts WHERE id=?1",
                [P1],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(ch, "h1");
        assert_eq!((orphan, dead), (1, 0));
        let v = blob_to_vec(&emb.unwrap()).unwrap();
        let src = parse_pgvector(&vec_text(1.0)).unwrap();
        assert!(cosine(&v, &src) > 0.9999999);
        assert_eq!(html, "<p>Hi, \"there\"</p>");
        assert_eq!(cluster.as_deref(), Some(C1));
        let (wp_id, emb2, meta, ctx_orphan): (Option<i64>, Option<Vec<u8>>, String, i64) = c
            .query_row(
                "SELECT wp_post_id, embedding, meta, is_contextual_orphan FROM posts WHERE id=?1",
                [P2],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((wp_id, emb2, ctx_orphan), (None, None, 1));
        assert_eq!(meta, "{\"k\":1}");
        let meta3: String = c
            .query_row("SELECT meta FROM posts WHERE id=?1", [P3], |r| r.get(0))
            .unwrap();
        assert_eq!(meta3, "{}");
        // clusters: pillar linked after posts
        let pillar: Option<String> = c
            .query_row("SELECT pillar_post_id FROM clusters", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pillar.as_deref(), Some(P1));
        // keywords: NULL volume, bool
        let (vol, pos, striking): (Option<i64>, Option<f64>, i64) = c
            .query_row(
                "SELECT volume, position, striking FROM keywords WHERE keyword='bottle'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((vol, pos, striking), (None, Some(3.2), 1));
        // job_runs: unfinished -> failed
        let (st, e): (String, Option<String>) = c
            .query_row(
                "SELECT status, error FROM job_runs WHERE kind='plan'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(st, "failed");
        assert!(e.unwrap().contains("unfinished"));
        Ok(())
    })
    .unwrap();

    // re-run is idempotent
    let rep2 = import_from_dir(&dir, &db, &MigrateOptions::new(key()), &|_| {}).unwrap();
    assert!(rep2.tables.iter().all(|t| t.inserted == 0));
    assert_eq!(
        rep2.tables
            .iter()
            .find(|t| t.table == "posts")
            .unwrap()
            .ignored,
        3
    );

    // verification: counts + embeddings
    let v = verify(&DirSource::new(&dir), &db, &MigrateOptions::new(key()), 100).unwrap();
    assert!(v.ok, "{v}");
    assert_eq!(v.sampled_embeddings, 2);
    assert!(v.min_cosine > 0.999999);
    db.close();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn site_filter_dry_run_and_fk_skips() {
    let dir = fixture_dir();
    let db = open_db(&dir);
    let mut o = MigrateOptions::new(key());
    o.site = Some(S1.into());
    o.dry_run = true;
    let rep = import_from_dir(&dir, &db, &o, &|_| {}).unwrap();
    let t = |r: &MigrateReport, n: &str| r.tables.iter().find(|t| t.table == n).unwrap().clone();
    assert_eq!(t(&rep, "sites").source_rows, 1);
    assert_eq!(t(&rep, "posts").source_rows, 2);
    assert_eq!(t(&rep, "paragraphs").source_rows, 2);
    assert_eq!(t(&rep, "job_runs").source_rows, 3); // 2 for S1 + 1 global
    assert_eq!(t(&rep, "users").source_rows, 1);
    let n: i64 = db
        .read_blocking(|c| {
            c.query_row("SELECT count(*) FROM posts", [], |r| r.get(0))
                .map_err(err)
        })
        .unwrap();
    assert_eq!(n, 0);

    // real site-filtered import
    o.dry_run = false;
    let rep = import_from_dir(&dir, &db, &o, &|_| {}).unwrap();
    assert_eq!(t(&rep, "posts").inserted, 2);
    assert_eq!(t(&rep, "paragraphs").inserted, 2);
    let v = verify(&DirSource::new(&dir), &db, &o, 100).unwrap();
    assert!(v.ok, "{v}");

    // tables subset without parents: rows are skipped by the FK, not fatal
    let db2 = Db::open(&dir.join("v2b").join("siloforge.db")).unwrap();
    let mut o2 = MigrateOptions::new(key());
    o2.tables = Some(vec!["paragraphs".into()]);
    o2.batch_size = 1;
    let rep = import_from_dir(&dir, &db2, &o2, &|_| {}).unwrap();
    assert_eq!(t(&rep, "paragraphs").skipped, 3);
    assert!(t(&rep, "paragraphs").first_error.is_some());

    // validation
    let mut bad = MigrateOptions::new(key());
    bad.site = Some("1' OR '1'='1".into());
    assert!(import_from_dir(&dir, &db2, &bad, &|_| {}).is_err());
    let mut bad = MigrateOptions::new(key());
    bad.tables = Some(vec!["pg_shadow".into()]);
    assert!(import_from_dir(&dir, &db2, &bad, &|_| {}).is_err());
    assert!(
        import_from_dir(
            &dir.join("missing"),
            &db2,
            &MigrateOptions::new(key()),
            &|_| {}
        )
        .is_err()
    );
    db.close();
    db2.close();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hostile_csv_never_panics() {
    let dir = std::env::temp_dir().join(format!("sf-ops-mig-h-{}", sf_core::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("users.csv"),
        "id,email\n\"unterminated,x\nä,\u{0}\n",
    )
    .unwrap();
    std::fs::write(dir.join("sites.csv"), "id\n").unwrap();
    std::fs::write(dir.join("posts.csv"), [0xff, 0xfe, 0x00, b'\n', b'a']).unwrap();
    std::fs::write(dir.join("paragraphs.csv"), "").unwrap();
    let db = open_db(&dir);
    let _ = import_from_dir(&dir, &db, &MigrateOptions::new(key()), &|_| {});
    let _ = verify(&DirSource::new(&dir), &db, &MigrateOptions::new(key()), 10);
    db.close();
    let _ = std::fs::remove_dir_all(dir);
}

/// Live v1 check (read-only): needs Docker and the `siloforge_pg` container. Run with `--ignored`.
#[test]
#[ignore]
fn live_docker_dry_run_counts() {
    let src = DockerSource::default();
    src.check().unwrap();
    let dir = std::env::temp_dir().join(format!("sf-ops-mig-live-{}", sf_core::new_id()));
    let db = open_db(&dir);
    let mut o = MigrateOptions::new(key());
    o.dry_run = true;
    let rep = import(&src, &db, &o, &|_| {}).unwrap();
    println!("{rep}");
    assert!(
        rep.tables
            .iter()
            .find(|t| t.table == "sites")
            .unwrap()
            .source_rows
            > 0
    );
    db.close();
    let _ = std::fs::remove_dir_all(dir);
}
