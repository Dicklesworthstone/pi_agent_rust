#[cfg(test)]
mod tests {
    use asupersync::http::client::HttpClient;
    use asupersync::http::Method;
    use asupersync::http::Url;

    #[test]
    fn test_client_api() {
        let client = HttpClient::new();
        let url: Url = "http://example.com".parse().unwrap();
        let _req = client.request(Method::Post, url); 
    }
}
