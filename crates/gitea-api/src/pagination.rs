//! Bounded continuation metadata; upstream links never become request targets.

use reqwest::header::HeaderMap;
use serde::Serialize;
use url::Url;

const MAX_LINK_BYTES: usize = 8 * 1024;
const MAX_LINKS: usize = 16;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Pagination {
    pub next_page: Option<u32>,
    pub total_count: Option<u64>,
    /// `None` means that the response does not establish exhaustion.
    pub complete: Option<bool>,
}

impl Pagination {
    pub(crate) fn from_headers(headers: &HeaderMap, request_url: &Url) -> Self {
        let current = request_url
            .query_pairs()
            .find(|(key, _)| key == "page")
            .and_then(|(_, value)| value.parse::<u32>().ok())
            .unwrap_or(1)
            .max(1);
        let links = continuation_links(headers, request_url, current);
        let has_more = single_header(headers, "x-hasmore").and_then(|value| match value {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        });
        let next_page = links.and_then(|(next, _)| next).or_else(|| {
            (has_more == Some(true))
                .then(|| current.checked_add(1))
                .flatten()
        });
        let complete = if next_page.is_some() {
            Some(false)
        } else if has_more == Some(false) || links.is_some_and(|(_, last)| last == Some(current)) {
            Some(true)
        } else {
            None
        };
        Self {
            next_page,
            total_count: number_header(headers, "x-total-count")
                .or_else(|| number_header(headers, "x-total")),
            complete,
        }
    }

    pub(crate) fn observe_empty_page(&mut self, empty: bool) {
        if empty && self.next_page.is_none() {
            self.complete = Some(true);
        }
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    (values.next().is_none() && first.len() <= MAX_LINK_BYTES).then_some(first)
}

fn number_header(headers: &HeaderMap, name: &str) -> Option<u64> {
    let value = single_header(headers, name)?;
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn continuation_links(
    headers: &HeaderMap,
    request_url: &Url,
    current: u32,
) -> Option<(Option<u32>, Option<u32>)> {
    let mut bytes = 0_usize;
    let mut count = 0;
    let mut next = None;
    let mut last = None;
    for header in headers.get_all("link") {
        bytes = bytes.checked_add(header.as_bytes().len())?;
        if bytes > MAX_LINK_BYTES {
            return None;
        }
        for entry in header.to_str().ok()?.split(',') {
            count += 1;
            if count > MAX_LINKS {
                return None;
            }
            let (target, parameters) = entry.trim().strip_prefix('<')?.split_once('>')?;
            let url = request_url.join(target).ok()?;
            if url.origin() != request_url.origin()
                || url.path() != request_url.path()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return None;
            }
            let mut pages = url.query_pairs().filter(|(key, _)| key == "page");
            let page = pages.next()?.1.parse::<u32>().ok()?;
            if page == 0 || pages.next().is_some() {
                return None;
            }
            let relation = parameters
                .split(';')
                .filter_map(|part| part.trim().split_once('='))
                .find(|(key, _)| *key == "rel")?
                .1
                .trim_matches('"');
            for relation in relation.split_ascii_whitespace() {
                match relation {
                    "next" if page > current && next.is_none() => next = Some(page),
                    "last" if page >= current && last.is_none() => last = Some(page),
                    "next" | "last" => return None,
                    _ => {}
                }
            }
        }
    }
    (count > 0).then_some((next, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_is_normalized_without_returning_or_following_urls() {
        let request = Url::parse("https://example.test/api/v1/repos?page=1&limit=100").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("link", "<https://example.test/api/v1/repos?page=2&limit=100>; rel=\"next\", <https://example.test/api/v1/repos?page=3&limit=100>; rel=\"last\"".parse().unwrap());
        headers.insert("x-total-count", "125".parse().unwrap());
        let pagination = Pagination::from_headers(&headers, &request);
        assert_eq!(
            pagination,
            Pagination {
                next_page: Some(2),
                total_count: Some(125),
                complete: Some(false)
            }
        );
        let serialized = serde_json::to_string(&pagination).unwrap();
        assert!(!serialized.contains("https://"));
    }

    #[test]
    fn missing_invalid_and_foreign_continuation_does_not_claim_exhaustion() {
        let request = Url::parse("https://example.test/api/v1/repos?page=1").unwrap();
        for link in [
            "",
            "malformed",
            "<https://other.test/api/v1/repos?page=2>; rel=\"next\"",
            "</other?page=2>; rel=\"next\"",
            "<?page=0>; rel=\"next\"",
            "<?page=2&page=3>; rel=\"next\"",
            "<?page=1>; rel=\"next\"",
        ] {
            let mut headers = HeaderMap::new();
            if !link.is_empty() {
                headers.insert("link", link.parse().unwrap());
            }
            let pagination = Pagination::from_headers(&headers, &request);
            assert_eq!(pagination.next_page, None);
            assert_eq!(pagination.complete, None);
        }
    }

    #[test]
    fn header_work_is_bounded_and_ambiguous_numbers_are_not_reported() {
        let request = Url::parse("https://example.test/api/v1/repos?page=1").unwrap();
        for link in [
            "x".repeat(MAX_LINK_BYTES + 1),
            "<?page=1>; rel=\"first\",".repeat(MAX_LINKS + 1),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("link", link.parse().unwrap());
            headers.append("x-total-count", "5".parse().unwrap());
            headers.append("x-total-count", "6".parse().unwrap());
            assert_eq!(
                Pagination::from_headers(&headers, &request),
                Pagination {
                    next_page: None,
                    total_count: None,
                    complete: None,
                }
            );
        }
        for count in ["-1", "not-a-count", "18446744073709551616"] {
            let mut headers = HeaderMap::new();
            headers.insert("x-total", count.parse().unwrap());
            headers.insert("x-hasmore", "true".parse().unwrap());
            assert_eq!(
                Pagination::from_headers(&headers, &request),
                Pagination {
                    next_page: Some(2),
                    total_count: None,
                    complete: Some(false),
                }
            );
        }
    }

    #[test]
    fn known_last_and_empty_pages_are_complete_but_short_pages_are_unknown() {
        let request = Url::parse("https://example.test/api/v1/repos?page=2&limit=100").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("link", "<?page=2>; rel=\"last\"".parse().unwrap());
        assert_eq!(
            Pagination::from_headers(&headers, &request).complete,
            Some(true)
        );
        headers.clear();
        let mut pagination = Pagination::from_headers(&headers, &request);
        pagination.observe_empty_page(false);
        assert_eq!(pagination.complete, None);
        pagination.observe_empty_page(true);
        assert_eq!(pagination.complete, Some(true));
    }
}
