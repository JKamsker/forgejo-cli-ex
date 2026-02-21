use regex::Regex;

pub fn html_decode(s: &str) -> String {
    html_escape::decode_html_entities(s).to_string()
}

pub fn get_html_attribute_value(html: &str, attribute_name: &str) -> Option<String> {
    let pattern = format!(
        r#"{}\s*=\s*(?:"([^"]*)"|'([^']*)')"#,
        regex::escape(attribute_name)
    );
    let re = Regex::new(&pattern).ok()?;
    let caps = re.captures(html)?;
    let value = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str())?;
    Some(html_decode(value))
}

pub fn get_csrf_from_login_html(html: &str) -> Option<String> {
    let re = Regex::new(r#"name="_csrf"\s+value="([^"]+)""#).ok()?;
    let caps = re.captures(html)?;
    let value = caps.get(1)?.as_str();
    Some(html_decode(value))
}

pub fn get_csrf_token_from_html(html: &str) -> Option<String> {
    // Forgejo/Gitea pages typically embed the CSRF token in `window.config`.
    let patterns = [
        r#"csrfToken:\s*'([^']+)'"#,
        r#"csrfToken:\s*"([^"]+)""#,
        // Some pages also expose it via htmx headers on the <body>.
        r#"hx-headers=['"][^'"]*"x-csrf-token"\s*:\s*"([^"]+)"[^'"]*['"]"#,
    ];

    for pattern in patterns {
        let re = Regex::new(pattern).ok()?;
        if let Some(caps) = re.captures(html) {
            if let Some(value) = caps.get(1).map(|m| m.as_str()) {
                if !value.trim().is_empty() {
                    return Some(html_decode(value));
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_is_extracted_and_decoded() {
        let html = r#"<input type="hidden" name="_csrf" value="abc&amp;123">"#;
        assert_eq!(get_csrf_from_login_html(html).as_deref(), Some("abc&123"));
    }

    #[test]
    fn csrf_token_is_extracted_from_page_script() {
        let html = r#"window.config = {csrfToken: 'abc&amp;123', other: 1};"#;
        assert_eq!(get_csrf_token_from_html(html).as_deref(), Some("abc&123"));
    }

    #[test]
    fn csrf_token_is_extracted_from_hx_headers() {
        let html = r#"<body hx-headers='{"x-csrf-token": "abc&amp;123"}'>"#;
        assert_eq!(get_csrf_token_from_html(html).as_deref(), Some("abc&123"));
    }

    #[test]
    fn data_attribute_is_extracted_and_decoded() {
        let html = r#"<div data-initial-post-response="{&quot;a&quot;:1}"></div>"#;
        assert_eq!(
            get_html_attribute_value(html, "data-initial-post-response").as_deref(),
            Some(r#"{"a":1}"#)
        );
    }

    #[test]
    fn data_attribute_accepts_single_quotes_and_whitespace() {
        let html = r#"<div data-initial-post-response = '{&quot;a&quot;:1}'></div>"#;
        assert_eq!(
            get_html_attribute_value(html, "data-initial-post-response").as_deref(),
            Some(r#"{"a":1}"#)
        );
    }
}
