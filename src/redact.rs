const URL_TRAILING_PUNCTUATION: [char; 10] = [')', ']', '}', '>', ',', ';', ':', '.', '"', '\''];

/// RPC providers keep the API key in the URL path or query, and HTTP errors print the whole URL.
pub(crate) fn redact_urls(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        redacted.push_str(head);
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(URL_TRAILING_PUNCTUATION);
        let authority = &url[..url.find(['/', '?', '#']).unwrap_or(url.len())];
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        redacted.push_str(host);
        if host.len() < url.len() {
            redacted.push_str("/<redacted>");
        }
        rest = &tail[url.len()..];
    }
    redacted.push_str(rest);
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_keeps_only_its_host() {
        assert_eq!(
            redact_urls("error sending request for url (https://rpc.example.com/?api-key=SECRET): timed out"),
            "error sending request for url (https://rpc.example.com/<redacted>): timed out"
        );
        assert_eq!(
            redact_urls(
                "http://user:SECRET@node.example:8899/SECRET, then https://other.example/x"
            ),
            "http://node.example:8899/<redacted>, then https://other.example/<redacted>"
        );
    }

    #[test]
    fn text_without_a_secret_in_a_url_is_unchanged() {
        assert_eq!(redact_urls("node down"), "node down");
        assert_eq!(
            redact_urls("refused by https://rpc.example.com."),
            "refused by https://rpc.example.com."
        );
    }
}
