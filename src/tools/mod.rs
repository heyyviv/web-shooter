pub mod crawl;
pub mod fetch;
pub mod links;
pub mod render;

/// The text ranked against: the query plus any keywords, or empty if neither.
pub fn query_text(query: Option<&str>, keywords: Option<&[String]>) -> String {
    let mut parts: Vec<&str> = query.into_iter().collect();
    parts.extend(keywords.unwrap_or_default().iter().map(String::as_str));
    parts.join(" ").trim().to_string()
}
