use crw_crawl::sitemap::parse_sitemap;

#[test]
fn sitemap_index_nested() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://example.com/sitemap1.xml</loc></sitemap>
  <sitemap><loc>https://example.com/sitemap2.xml</loc></sitemap>
</sitemapindex>"#;
    let res = parse_sitemap(xml);
    assert_eq!(res.child_sitemaps.len(), 2);
    assert!(res.page_urls.is_empty());
    assert!(
        res.child_sitemaps
            .contains(&"https://example.com/sitemap1.xml".to_string())
    );
    assert!(
        res.child_sitemaps
            .contains(&"https://example.com/sitemap2.xml".to_string())
    );
}

#[test]
fn sitemap_empty_urlset() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
</urlset>"#;
    let res = parse_sitemap(xml);
    assert!(res.page_urls.is_empty());
    assert!(res.child_sitemaps.is_empty());
}

#[test]
fn sitemap_invalid_xml() {
    let xml = "this is not xml at all <><><>";
    let res = parse_sitemap(xml);
    assert!(res.page_urls.is_empty());
    assert!(res.child_sitemaps.is_empty());
}

#[test]
fn sitemap_standard_urlset() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/page1</loc></url>
  <url><loc>https://example.com/page2</loc></url>
  <url><loc>https://example.com/page3</loc></url>
</urlset>"#;
    let res = parse_sitemap(xml);
    assert_eq!(res.page_urls.len(), 3);
    assert!(res.child_sitemaps.is_empty());
}

#[test]
fn sitemap_with_extra_elements() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url>
    <loc>https://example.com/page1</loc>
    <lastmod>2024-01-01</lastmod>
    <changefreq>daily</changefreq>
    <priority>0.8</priority>
  </url>
</urlset>"#;
    let res = parse_sitemap(xml);
    assert_eq!(res.page_urls.len(), 1);
    assert_eq!(res.page_urls[0], "https://example.com/page1");
}

#[test]
fn sitemap_whitespace_in_loc() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>
    https://example.com/page1
  </loc></url>
</urlset>"#;
    let res = parse_sitemap(xml);
    assert_eq!(res.page_urls.len(), 1);
    assert_eq!(res.page_urls[0], "https://example.com/page1");
}

#[test]
fn sitemap_empty_string() {
    let res = parse_sitemap("");
    assert!(res.page_urls.is_empty());
    assert!(res.child_sitemaps.is_empty());
}
