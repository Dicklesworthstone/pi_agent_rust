#[cfg(test)]
mod tests {
    use asupersync::http::Method;
    use asupersync::http::Url;
    use asupersync::http::client::HttpClient;

    #[test]
    fn test_client_api() {
        let client = HttpClient::new();
        let url: Url = "http://example.com".parse().unwrap();
        let _req = client.request(Method::Post, url);
    }
}
